import * as vscode from 'vscode';
import { TextDecoder } from 'util';
import { CancellationToken, DebugConfiguration, ProviderResult, WorkspaceFolder } from 'vscode';

export function activate(context: vscode.ExtensionContext) {

	const dbgeeConnector = new DbgeeConnector(vscode.workspace.fs);

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

	const debuggerConfigs = vscode.extensions.getExtension("nullpo-head.dbgee")?.packageJSON["contributes"]["debuggers"] as DebuggerConfig[];
	for (const debuggerConfig of debuggerConfigs) {
		vscode.debug.registerDebugConfigurationProvider(debuggerConfig.type, {
			resolveDebugConfiguration: (folder: WorkspaceFolder | undefined, config: DebugConfiguration, token?: CancellationToken): ProviderResult<DebugConfiguration> => {
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

class DbgeeConnector {
	fs: vscode.FileSystem;
	connected: boolean = false;
	retrievedProperties: Set<String>;
	private attachInformation: DbgeeAttachInformation | undefined;

	constructor(fs: vscode.FileSystem) {
		this.fs = fs;
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
		this.attachInformation = JSON.parse(new TextDecoder("utf-8").decode(await this.fs.readFile(vscode.Uri.file(fifoPath)))) as DbgeeAttachInformation;
		this.retrievedProperties = new Set<String>();
	}
}

interface DebuggerConfig {
	type: string;
	languages: string[];
	initialConfigurations: vscode.DebugConfiguration[];
}

interface DbgeeAttachInformation {
	debuggeeTypeHint?: string;
	pid?: string;
	debuggerPort?: string;
	programName?: string;
	error?: string;
}

export function deactivate() { }
