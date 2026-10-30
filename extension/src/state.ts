/** Run state of one agent, rebuilt from omp's extension hooks (SPEC § Calcul de l'état). */
export type State = "idle" | "working" | "blocked" | "error";

export type StateEvent =
	| { type: "session_start" }
	| { type: "agent_start" }
	/** An approval prompt or an `ask` question is waiting on the user; `id` is its tool call. */
	| { type: "block"; id: string }
	| { type: "unblock"; id: string }
	| { type: "agent_end"; willContinue: boolean; lastStopReason: string | undefined };

export interface Tracker {
	state: State;
	/** Tool calls currently waiting on the user: the agent stays blocked until all are answered. */
	blocking: Set<string>;
}

export function reduce(t: Tracker, e: StateEvent): State {
	switch (e.type) {
		case "session_start":
			t.blocking.clear();
			t.state = "idle";
			break;
		case "agent_start":
			t.blocking.clear();
			t.state = "working";
			break;
		case "block":
			t.blocking.add(e.id);
			t.state = "blocked";
			break;
		case "unblock":
			t.blocking.delete(e.id);
			t.state = t.blocking.size > 0 ? "blocked" : "working";
			break;
		case "agent_end":
			t.blocking.clear();
			if (e.willContinue) t.state = "working";
			else t.state = e.lastStopReason === "error" ? "error" : "idle";
			break;
	}
	return t.state;
}
