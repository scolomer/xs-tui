import { describe, expect, test } from "bun:test";
import { reduce, type Tracker } from "./state";

function fresh(): Tracker {
	const t: Tracker = { state: "idle", blocking: new Set() };
	reduce(t, { type: "session_start" });
	return t;
}

describe("reduce", () => {
	test("stays blocked until every pending prompt is answered", () => {
		const t = fresh();
		reduce(t, { type: "agent_start" });
		reduce(t, { type: "block", id: "a" });
		reduce(t, { type: "block", id: "b" });
		expect(reduce(t, { type: "unblock", id: "a" })).toBe("blocked");
		expect(reduce(t, { type: "unblock", id: "b" })).toBe("working");
	});

	test("an error turn is replaced by the next turn", () => {
		const t = fresh();
		reduce(t, { type: "agent_start" });
		expect(reduce(t, { type: "agent_end", willContinue: false, lastStopReason: "error" })).toBe("error");
		expect(reduce(t, { type: "agent_start" })).toBe("working");
	});

	test("a turn that will continue stays working", () => {
		const t = fresh();
		reduce(t, { type: "agent_start" });
		expect(reduce(t, { type: "agent_end", willContinue: true, lastStopReason: "stop" })).toBe("working");
	});

	test("an interrupted turn ends idle", () => {
		const t = fresh();
		reduce(t, { type: "agent_start" });
		expect(reduce(t, { type: "agent_end", willContinue: false, lastStopReason: "aborted" })).toBe("idle");
	});
});
