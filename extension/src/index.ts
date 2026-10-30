/**
 * agent-tui publisher: writes each agent's run state to `~/.omp/run/agent-tui/<pid>/<agentId>.json`
 * so the agent-tui dashboard can show it (SPEC.md § Extension omp).
 *
 * The module is imported once per process and the factory is rebound for every subagent: module
 * variables are shared between the main session and its subagents, closure variables are per agent.
 */
import * as fs from "node:fs/promises";
import { rmSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import { reduce, type Tracker } from "./state";

const RUN_DIR = path.join(os.homedir(), ".omp", "run", "agent-tui");
const FORMAT_VERSION = 1;
const TITLE_POLL_MS = 5000;

/** Whether this process's main session publishes; subagents follow it (SPEC § Activation). */
let rootPublishes = false;

const pidDir = path.join(RUN_DIR, String(process.pid));

export default function agentTui(pi: ExtensionAPI) {
	pi.setLabel("agent-tui");

	let enabled = false;
	const tracker: Tracker = { state: "idle", blocking: new Set() };
	let startedAt = "";
	let lastTitle = "";
	let file: string | undefined;
	let titleTimer: Timer | undefined;
	let exitHookInstalled = false;

	function record(ctx: ExtensionContext) {
		const session = process.env.ZELLIJ_SESSION_NAME;
		const paneId = process.env.ZELLIJ_PANE_ID;
		return {
			v: FORMAT_VERSION,
			pid: process.pid,
			agent: {
				kind: ctx.agent.kind,
				id: ctx.agent.id,
				name: ctx.agent.name,
				depth: ctx.agent.depth,
				parentId: ctx.agent.parentId ?? null,
			},
			sessionId: ctx.sessionManager.getSessionId(),
			cwd: ctx.cwd,
			title: ctx.sessionManager.getSessionName() ?? "",
			state: tracker.state,
			zellij: session && paneId ? { session, paneId } : null,
			startedAt,
			updatedAt: new Date().toISOString(),
		};
	}

	async function publish(ctx: ExtensionContext) {
		if (!enabled) return;
		const rec = record(ctx);
		const name = `${ctx.agent.id.replace(/[^A-Za-z0-9_.-]/g, "_")}.json`;
		const target = path.join(pidDir, name);
		const tmp = path.join(pidDir, `.${name}.tmp`);
		try {
			await fs.mkdir(pidDir, { recursive: true });
			await fs.writeFile(tmp, JSON.stringify(rec));
			await fs.rename(tmp, target);
			file = target;
			lastTitle = rec.title;
		} catch {
			// Never fail omp over the dashboard.
		}
	}

	async function remove(ctx: ExtensionContext) {
		try {
			if (ctx.agent.kind === "main") await fs.rm(pidDir, { recursive: true, force: true });
			else if (file) await fs.rm(file, { force: true });
		} catch {
			// Ignored, see publish.
		}
		file = undefined;
	}

	pi.on("session_start", async (_event, ctx) => {
		if (ctx.agent.kind === "main") {
			enabled = ctx.mode === "tui";
			rootPublishes = enabled;
		} else {
			enabled = rootPublishes;
		}
		if (!enabled) return;
		startedAt = new Date().toISOString();
		reduce(tracker, { type: "session_start" });
		await publish(ctx);
		if (ctx.agent.kind !== "main") return;
		if (titleTimer) ctx.clearTimer(titleTimer);
		titleTimer = ctx.setInterval(() => {
			if ((ctx.sessionManager.getSessionName() ?? "") !== lastTitle) void publish(ctx);
		}, TITLE_POLL_MS);
		if (!exitHookInstalled) {
			exitHookInstalled = true;
			process.once("exit", () => rmSync(pidDir, { recursive: true, force: true }));
		}
	});

	pi.on("session_switch", async (_event, ctx) => {
		await publish(ctx);
	});

	pi.on("agent_start", async (_event, ctx) => {
		if (!enabled) return;
		reduce(tracker, { type: "agent_start" });
		await publish(ctx);
	});

	pi.on("tool_approval_requested", async (event, ctx) => {
		if (!enabled) return;
		reduce(tracker, { type: "block", id: event.toolCallId });
		await publish(ctx);
	});

	pi.on("tool_approval_resolved", async (event, ctx) => {
		if (!enabled) return;
		reduce(tracker, { type: "unblock", id: event.toolCallId });
		await publish(ctx);
	});

	pi.on("tool_execution_start", async (event, ctx) => {
		if (!enabled || event.toolName !== "ask") return;
		reduce(tracker, { type: "block", id: event.toolCallId });
		await publish(ctx);
	});

	pi.on("tool_execution_end", async (event, ctx) => {
		if (!enabled || event.toolName !== "ask") return;
		reduce(tracker, { type: "unblock", id: event.toolCallId });
		await publish(ctx);
	});

	pi.on("agent_end", async (event, ctx) => {
		if (!enabled) return;
		const lastAssistant = event.messages.findLast(m => m.role === "assistant");
		const lastStopReason = lastAssistant && "stopReason" in lastAssistant ? lastAssistant.stopReason : undefined;
		const willContinue = !!event.willContinue;
		reduce(tracker, { type: "agent_end", willContinue, lastStopReason });
		if (ctx.agent.kind === "sub" && !willContinue) await remove(ctx);
		else await publish(ctx);
	});

	pi.on("session_shutdown", async (_event, ctx) => {
		if (!enabled) return;
		enabled = false;
		if (titleTimer) ctx.clearTimer(titleTimer);
		titleTimer = undefined;
		await remove(ctx);
	});
}
