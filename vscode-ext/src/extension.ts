import * as vscode from 'vscode';
import { CancellationToken, DebugConfiguration, ProviderResult, WorkspaceFolder } from 'vscode';
import * as child_process from 'child_process';
import * as process from 'process';
import * as nodeFs from 'fs';
import * as nodeUtil from 'util';
import * as net from 'net';

const PROTOCOL_VERSION = "0.2.0";

type Deactivate = () => void;
type RegisterDeactivate = (deactivate: Deactivate) => void;
const deactivators: Deactivate[] = [];
const registerDeactivate = (deactivate: Deactivate) => { deactivators.push(deactivate); };

export function activate(context: vscode.ExtensionContext) {

	const dbgeeConnector = new DbgeeConnector();
	const attachInfoCommandFactory = (information: keyof DbgeeAttachInformation) => (async () => {
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
	private retrievedProperties: Set<String>;
	private attachInformation: DbgeeAttachInformation | undefined;

	constructor() {
		this.retrievedProperties = new Set<String>();
	}

	async getAttachInformation(key: keyof DbgeeAttachInformation): Promise<String | undefined> {
		if (this.attachInformation === undefined || this.retrievedProperties.has(key)) {
			// reading the same key twice indicates that we're in a new session
			await this.refreshAttachInformation();
		}
		this.retrievedProperties.add(key);
		return this.attachInformation![key];
	}

	async refreshAttachInformation() {
		const fifoPath = "/tmp/dbgee-vscode-debuggees";
		makeFifoUnlessExists(fifoPath);
		this.attachInformation = JSON.parse(await readFifo(fifoPath)) as DbgeeAttachInformation;
		this.retrievedProperties = new Set<String>();
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
		const _listen = async () => {
			while (true) {
				const fifoPath = await this.requestFifoPath.path;
				await makeFifoUnlessExists(fifoPath);
				const request = JSON.parse(await readFifo(await this.requestFifoPath.path)) as DbgeeAttachRequest;
				const config = this.debuggerConfigFactory.getDebuggerConfigurationForRequest(request);
				if (!config) {
					continue;
				}
				if (!this.debugSessionTracker.isDebugSessionActive) {
					vscode.debug.startDebugging(vscode.workspace.workspaceFolders?.[0], config);
				}
			}
		};
		_listen().catch(reason => vscode.window.showErrorMessage(`[Dbgee] Error on requests listening. Active debugger session is disabled. Error: ${reason}`));
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
			const findGitSocketPath = `awk '($2 == ${process.pid} && $5 == "unix" && $9 ~ /git/) { print $9; }'`;
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
						console.log("session active");
					},
					onWillStopSession: () => {
						self._isDebugSessionActive = false;
						console.log("session inactive");
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
		for (const debugConfig of this.getInitialConfigurations()) {
			if (debugConfig.initialConfigurations[0].type === request.debuggerType) {
				return debugConfig.initialConfigurations[0];
			}
		}
		vscode.window.showErrorMessage(`Dbgee command has requested unknown debugger: ${request.debuggerType}`);
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

async function readFifo(path: string): Promise<string> {
	return new Promise<string>((resolve, reject) => {
		nodeFs.open(path, nodeFs.constants.O_RDONLY | nodeFs.constants.O_NONBLOCK, (err, fd) => {
			if (err) {
				reject(err);
			}
			const pipeAsSocket = new net.Socket({ fd });
			pipeAsSocket.on("data", (data) => {
				const content = data.toString();
				resolve(content);
			});
		});
	});
}

async function makeFifoUnlessExists(path: string) {
	const exec = nodeUtil.promisify(child_process.exec);
	if (!nodeFs.existsSync(path)) {
		await exec(`mkfifo ${path}`);
	}
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

export function deactivate() {
	for (const deactivate of deactivators) {
		deactivate();
	}
}
