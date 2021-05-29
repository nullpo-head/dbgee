import * as vscode from 'vscode';
import { CancellationToken, DebugConfiguration, ProviderResult, WorkspaceFolder } from 'vscode';
import * as child_process from 'child_process';
import * as process from 'process';
import * as nodeFs from 'fs';
import * as nodeUtil from 'util';
import * as net from 'net';

const PROTOCOL_VERSION = "1.1.0";

type Deactivate = () => void;
type RegisterDeactivate = (deactivate: Deactivate) => void;
const deactivators: Deactivate[] = [];
const registerDeactivate = (deactivate: Deactivate) => { deactivators.push(deactivate); };

export function activate(context: vscode.ExtensionContext) {

	const dbgeeConnector = new DbgeeConnector();
	const attachInfoCommandFactory = (information: keyof DbgeeAttachInformation) => (async () => {
		logger.trace(`getting attach information: ${information}`);
		const info = await dbgeeConnector.getAttachInformation(information);

		if (!info) {
			vscode.window.showErrorMessage(`${information} is not found in the information given by dbgee.`);
			return '';
		}

		return info;
	});

	context.subscriptions.push(vscode.commands.registerCommand('dbgee.getPid', attachInfoCommandFactory("pid")));
	context.subscriptions.push(vscode.commands.registerCommand('dbgee.getDebuggerPort', attachInfoCommandFactory("debuggerPort")));
	context.subscriptions.push(vscode.commands.registerCommand('dbgee.getProgramName', attachInfoCommandFactory("programName")));

	const dbgeeDebuggerConfigurationFactory = new DbgeeDebuggerConfigurationFactory();
	const debugSessionTracker = new DebugSessionTracker();
	const dbgeeRequestListener = new DbgeeRequestListener(
		dbgeeDebuggerConfigurationFactory,
		debugSessionTracker,
		registerDeactivate
	);
	const dbgeeDebuggerConfigurationProvider = new DbgeeDebuggerConfigurationProvider(dbgeeDebuggerConfigurationFactory);

	debugSessionTracker.activate();
	dbgeeRequestListener.listen();
	dbgeeDebuggerConfigurationProvider.registerToVsCode();
}

class DbgeeConnector {
	private retrievedProperties: Set<string>;
	private attachInformation: DbgeeAttachInformation | undefined;

	constructor() {
		this.retrievedProperties = new Set<string>();
	}

	async getAttachInformation(key: keyof DbgeeAttachInformation): Promise<string | undefined> {
		if (!this.attachInformation || this.retrievedProperties.has(key)) {
			// reading the same key twice indicates that we're in a new session
			await this.refreshAttachInformation();
		}
		this.retrievedProperties.add(key);
		return this.attachInformation![key];
	}

	async refreshAttachInformation() {
		const fifoPath = "/tmp/dbgee-vscode-debuggees";
		makeFifoUnlessExists(fifoPath);
		logger.trace(`waiting attach information`);
		this.attachInformation = JSON.parse(await readFifo(fifoPath, 30_000)) as DbgeeAttachInformation;
		if (detectSemVerBreakingChange(PROTOCOL_VERSION, this.attachInformation.protocolVersion)) {
			throw new Error("incompatible protocol version");
		}
		this.retrievedProperties = new Set<string>();
		logger.trace(`got attach information ${JSON.stringify(this.attachInformation)}`);
	}
}

class DbgeeRequestListener {
	private requestFifoPath: DbgeeRequestFifoPath;
	private debugSessionTracker: DebugSessionTracker;
	private debuggerConfigFactory: DbgeeDebuggerConfigurationFactory;

	constructor(debuggerConfigFactory: DbgeeDebuggerConfigurationFactory, debugSessionTracker: DebugSessionTracker, registerDeactivate: RegisterDeactivate) {
		this.requestFifoPath = new DbgeeRequestFifoPath();
		this.debuggerConfigFactory = debuggerConfigFactory;
		this.debugSessionTracker = debugSessionTracker;
		registerDeactivate(() => {
			const _deactivate = async () => {
				const path = await this.requestFifoPath.path;
				nodeFs.unlink(path, (_) => { });
			};
			_deactivate();
		});
	}

	listen() {
		let listeningLoop = 0;
		const _listen = async () => {
			while (true) {
				const fifoPath = await this.requestFifoPath.path;
				await makeFifoUnlessExists(fifoPath);
				logger.trace(`[${listeningLoop}] listening`);
				const request = JSON.parse(await readFifo(await this.requestFifoPath.path)) as DbgeeAttachRequest;
				if (detectSemVerBreakingChange(PROTOCOL_VERSION, request.protocolVersion)) {
					throw new Error("incompatible protocol version");
				}
				logger.trace(`[${listeningLoop}] got attach request: ${JSON.stringify(request)}`);
				const config = this.debuggerConfigFactory.getDebuggerConfigurationForRequest(request);
				if (!config) {
					logger.trace(`[${listeningLoop}] no config found for the config`);
					continue;
				}
				if (!this.debugSessionTracker.isDebugSessionActive) {
					logger.trace(`[${listeningLoop}] starting the debug session`);
					vscode.debug.startDebugging(vscode.workspace.workspaceFolders?.[0], config);
				}
				logger.trace(`[${listeningLoop}] end of listening finished`);
				listeningLoop++;
			}
		};
		_listen().catch(reason => logger.error(`Error on requests listening. Active debugger session is disabled. Error: ${reason}`));
	}
}

class DbgeeRequestFifoPath {
	private _path: Promise<string> | undefined;

	get path(): Promise<string> {
		if (!this._path) {
			this._path = this.getPath();
		}
		return this._path;
	}

	private async getPath(): Promise<string> {
		const vscodeId = await this.getVscodeSessionId();
		return `/tmp/dbgee-vscode-debuggee-for-${vscodeId}`;
	}

	private async getVscodeSessionId(): Promise<string> {
		////
		// Heuristics:
		// Each VSCode window seems to have one unique UNIX socket for Git IPC.
		// It can be retrieved by $VSCODE_GIT_IPC_HANDLE in shell sessions in the integrated terminal.
		// Use it to distinguish VSCode's windows
		////
		const lsofPromise = new Promise<string>((resolve, reject) => {
			const findGitSocketPath = `awk '($2 == ${process.pid} && $5 == "unix") { print $0; }' | grep -Eo '([^ \t]+git[^ \t]+)'`;
			child_process.exec(`basename $(lsof -U | ${findGitSocketPath})`, (error, stdout, _) => {
				if (error) {
					reject(error);
					return;
				}
				resolve(stdout);
			});
		});
		const gitIpcPath = (await lsofPromise).split("\n")[0];
		if (!gitIpcPath.endsWith(".sock")) {
			throw new Error("Failed to Git IPC path");
		}
		return gitIpcPath.slice(0, -5);
	}
}

class DebugSessionTracker {
	private _isDebugSessionActive: boolean = false;

	activate() {
		const self = this;
		vscode.debug.registerDebugAdapterTrackerFactory("*", {
			createDebugAdapterTracker(session: vscode.DebugSession) {
				return {
					onWillStartSession: () => {
						self._isDebugSessionActive = true;
						logger.trace("session active");
					},
					onWillStopSession: () => {
						self._isDebugSessionActive = false;
						logger.trace("session inactive");
					}
				};
			}
		});
	}

	get isDebugSessionActive(): boolean {
		return this._isDebugSessionActive;
	}
}

class DbgeeDebuggerConfigurationFactory {
	getDebuggerConfigurationForRequest(request: DbgeeAttachRequest): vscode.DebugConfiguration | undefined {
		if (vscode.workspace.workspaceFolders?.[0]) {
			const launchjson = vscode.workspace.getConfiguration("launch", vscode.workspace.workspaceFolders[0].uri);
			const configs = launchjson.get("configurations") as vscode.DebugConfiguration[] || [];
			for (const debugConfig of configs) {
				if (debugConfig.name.startsWith("(default)Dbgee:") && debugConfig.type === request.debuggerType) {
					return debugConfig;
				}
			}
		}
		for (const debugConfig of this.getInitialConfigurations()) {
			if (debugConfig.initialConfigurations[0].type === request.debuggerType) {
				return debugConfig.initialConfigurations[0];
			}
		}
		logger.error(`Dbgee command has requested unknown debugger: ${request.debuggerType}`);
		return;
	}

	getInitialConfigurations(): DebuggerConfig[] {
		return vscode.extensions.getExtension("nullpo-head.dbgee")?.packageJSON["contributes"]["debuggers"] as DebuggerConfig[];
	}
}

class DbgeeDebuggerConfigurationProvider {
	private factory: DbgeeDebuggerConfigurationFactory;

	constructor(factory: DbgeeDebuggerConfigurationFactory) {
		this.factory = factory;
	}

	registerToVsCode() {
		for (const debuggerConfig of this.factory.getInitialConfigurations()) {
			vscode.debug.registerDebugConfigurationProvider(debuggerConfig.type, {
				resolveDebugConfiguration: (_folder: WorkspaceFolder | undefined, config: DebugConfiguration, token?: CancellationToken): ProviderResult<DebugConfiguration> => {
					if (!config.type && !config.request && !config.name) {
						const editor = vscode.window.activeTextEditor;
						if (editor && debuggerConfig.languages.includes(editor.document.languageId)) {
							for (const prop of Object.keys(debuggerConfig.initialConfigurations[0])) {
								config[prop] = debuggerConfig.initialConfigurations[0][prop];
							}
						}
					}
					return config;
				}
			});
		}
	}
}

async function readFifo(path: string, timeout?: number): Promise<string> {
	return new Promise<string>((resolve, reject) => {
		nodeFs.open(path, nodeFs.constants.O_RDONLY | nodeFs.constants.O_NONBLOCK, (err, fd) => {
			logger.trace(`opened fifo: ${path}`);
			if (err) {
				logger.error(`unknown error happend during opening a fifo. path: ${path}`);
				reject(err);
			}

			let completed = false;
			const pipeAsSocket = new net.Socket({ fd });
			logger.trace(`fifo as socket: ${path}`);
			pipeAsSocket.on("data", (data) => {
				if (completed) {
					return;
				}
				completed = true;

				const content = data.toString();
				logger.trace(`fifo on data: ${path}, ${content}`);
				if (process.platform === "darwin") {
					// due to a bug of macOS, closing of fifo cannot be detected by libuv and Node.js
					// so, close it here manually.
					// https://github.com/golang/go/issues/24164
					pipeAsSocket.destroy();
				}
				resolve(content);
			});
			if (timeout) {
				setTimeout(() => {
					if (completed) {
						return;
					}
					completed = true;
					pipeAsSocket.destroy();
					logger.error("Waiting for attach information has timedout");
					reject("reading from fifo has timedout");
				}, timeout);
			}
		});
	});
}

async function makeFifoUnlessExists(path: string) {
	const exec = nodeUtil.promisify(child_process.exec);
	if (!nodeFs.existsSync(path)) {
		await exec(`mkfifo ${path}`);
	}
}

function detectSemVerBreakingChange(currentVer: string, requestedVer?: string): boolean {
	if (!requestedVer) {
		requestedVer = "1.0.0";
	}
	if (requestedVer.split(".")[0] === currentVer.split(".")[0]) {
		return false;
	}

	logger.error("Dbgee command is an incompatible version of this VSCode extension. Please upgrade Dbgee-vscode extension and the dbgee command to the latest versions.");
	return true;
}

interface DebuggerConfig {
	type: string;
	languages: string[];
	initialConfigurations: vscode.DebugConfiguration[];
}

interface DbgeeAttachInformation {
	protocolVersion?: string;
	pid?: string;
	debuggerPort?: string;
	programName?: string;
}

interface DbgeeAttachRequest {
	protocolVersion: string;
	debuggerType: string;
}


type LogLevel = "trace" | "debug" | "warn" | "error" | "off";
const LOG_LEVELS: LogLevel[] = ["trace", "debug", "warn", "error", "off"];

class Logger {
	private level: LogLevel;
	private hasAnyOutput: boolean;
	private outputChannel: vscode.OutputChannel | undefined;

	constructor(level: LogLevel) {
		this.level = level;
		this.hasAnyOutput = false;
	}

	private compareLogLevel(l1: LogLevel, l2: LogLevel): number {
		return LOG_LEVELS.indexOf(l1) - LOG_LEVELS.indexOf(l2);
	}

	log(s: string, level: LogLevel) {
		if (this.compareLogLevel(level, this.level) >= 0) {
			if (!this.hasAnyOutput) {
				this.hasAnyOutput = true;
				this.outputChannel = vscode.window.createOutputChannel("Dbgee-vscode");
			}
			this.outputChannel?.appendLine(`[${level}] ${s}`);
			console.log("[Dbgee] " + s);
		}
	}

	trace(s: string) {
		this.log(s, "trace");
	}

	error(s: string) {
		this.log(s, "error");
		vscode.window.showErrorMessage(`[Dbgee-vscode][Error] ${s}`);
	}
}
const logger = new Logger(vscode.workspace.getConfiguration("dbgee")["logLevel"] || "off");

export function deactivate() {
	for (const deactivate of deactivators) {
		deactivate();
	}
}
