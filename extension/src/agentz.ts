/**
 * agentz publisher: writes each agent's run state to `~/.omp/run/agentz/<pid>/<agentId>.json`
 * so the agz dashboard can show it (SPEC.md § Extension omp).
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

const RUN_DIR = path.join(os.homedir(), ".omp", "run", "agentz");
const FORMAT_VERSION = 1;
/** Polling period for what omp changes without an extension event: title and `/move` cwd. */
const POLL_MS = 500;

/** Whether this process's main session publishes; subagents follow it (SPEC § Activation). */
let rootPublishes = false;

const pidDir = path.join(RUN_DIR, String(process.pid));

export default function agentz(pi: ExtensionAPI) {
	pi.setLabel("agentz");

	let enabled = false;
	const tracker: Tracker = { state: "idle", blocking: new Set() };
	let startedAt = "";
	let lastTitle = "";
	let lastCwd = "";
	let file: string | undefined;
	let pollTimer: Timer | undefined;
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
			// Not `ctx.cwd`: a snapshot from when `ctx` was built, stale after `/move` for the timer.
			cwd: ctx.sessionManager.getCwd(),
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
			lastCwd = rec.cwd;
		} catch (error) {
			// Never fail omp over the dashboard; leave a trace in the omp log instead.
			pi.logger.warn("agentz: publish failed", { file: target, error: String(error) });
		}
	}

	async function remove(ctx: ExtensionContext) {
		try {
			if (ctx.agent.kind === "main") await fs.rm(pidDir, { recursive: true, force: true });
			else if (file) await fs.rm(file, { force: true });
		} catch (error) {
			pi.logger.warn("agentz: cleanup failed", { dir: pidDir, error: String(error) });
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
		pi.logger.debug("agentz: session_start", {
			agent: ctx.agent.id,
			kind: ctx.agent.kind,
			mode: ctx.mode,
			enabled,
		});
		if (!enabled) return;
		startedAt = new Date().toISOString();
		reduce(tracker, { type: "session_start" });
		await publish(ctx);
		if (ctx.agent.kind !== "main") return;
		if (pollTimer) ctx.clearTimer(pollTimer);
		// The title changes at rest after the first turn or a `/rename`; `/move` and `/wt` relocate
		// the session without any event.
		pollTimer = ctx.setInterval(() => {
			const title = ctx.sessionManager.getSessionName() ?? "";
			if (title !== lastTitle || ctx.sessionManager.getCwd() !== lastCwd) void publish(ctx);
		}, POLL_MS);
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
		if (pollTimer) ctx.clearTimer(pollTimer);
		pollTimer = undefined;
		await remove(ctx);
	});
}
