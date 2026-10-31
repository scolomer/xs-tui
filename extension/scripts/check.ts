/**
 * `mise run extension:check`: verifies that omp loads the installed agentz extension and that it
 * publishes. Starts a throwaway interactive omp (`--no-session`) in a pseudo-terminal, since the
 * extension only publishes in `tui` mode, waits for its record, then exits it with `/exit` and
 * checks that the record directory is removed.
 */
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const EXTENSION_DIR = path.resolve(import.meta.dir, "..");
const LINK = path.join(os.homedir(), ".omp", "agent", "extensions", "agentz");
const RUN_DIR = path.join(os.homedir(), ".omp", "run", "agentz");
const PUBLISH_TIMEOUT_MS = 60_000;
const EXIT_TIMEOUT_MS = 20_000;

/** A failed check: reported as a single line, after cleanup. */
class CheckFailed extends Error {}

async function until<T>(timeoutMs: number, probe: () => T | undefined): Promise<T | undefined> {
	const deadline = Date.now() + timeoutMs;
	while (Date.now() < deadline) {
		const value = probe();
		if (value !== undefined) return value;
		await Bun.sleep(200);
	}
	return undefined;
}

type Published = { dir: string; record: Record<string, any> };

/** Main record published by the omp process `pid`. */
function findRecord(pid: number): Published | undefined {
	const dir = path.join(RUN_DIR, String(pid));
	try {
		return { dir, record: JSON.parse(fs.readFileSync(path.join(dir, "Main.json"), "utf8")) };
	} catch {
		// Not published yet, or being replaced.
		return undefined;
	}
}

function checkLink() {
	let linked: string;
	try {
		linked = fs.realpathSync(LINK);
	} catch {
		throw new CheckFailed(`${LINK} is missing: run mise run extension:install`);
	}
	if (linked !== fs.realpathSync(EXTENSION_DIR)) {
		throw new CheckFailed(`${LINK} points to ${linked}, not to ${EXTENSION_DIR}`);
	}
	console.log(`✓ ${LINK} -> ${linked}`);
}

async function checkOmp() {
	const cwd = fs.mkdtempSync(path.join(os.tmpdir(), "agentz-check-"));
	let screen = "";
	const decoder = new TextDecoder();
	const omp = Bun.spawn(["omp", "--no-session"], {
		cwd,
		env: { ...process.env, TERM: process.env.TERM ?? "xterm-256color" },
		terminal: {
			cols: 120,
			rows: 40,
			data(_terminal, data) {
				screen = (screen + decoder.decode(data)).slice(-4000);
			},
		},
	});
	try {
		const published = await until(PUBLISH_TIMEOUT_MS, () => findRecord(omp.pid));
		if (!published) {
			const exited = omp.exitCode === null ? "" : ` (omp exited with code ${omp.exitCode})`;
			console.error(screen);
			throw new CheckFailed(
				`no record in ${RUN_DIR}/${omp.pid} after ${PUBLISH_TIMEOUT_MS / 1000} s${exited}`,
			);
		}
		const { dir, record } = published;
		if (record.v !== 1 || record.agent?.kind !== "main" || typeof record.sessionId !== "string") {
			throw new CheckFailed(`unexpected record in ${dir}: ${JSON.stringify(record)}`);
		}
		console.log(`✓ omp ${record.pid} publishes ${dir}/Main.json (state ${record.state})`);

		omp.terminal?.write("/exit\r");
		const removed = await until(EXIT_TIMEOUT_MS, () => (fs.existsSync(dir) ? undefined : true));
		if (!removed) throw new CheckFailed(`${dir} is still there ${EXIT_TIMEOUT_MS / 1000} s after /exit`);
		console.log(`✓ /exit removes ${dir}`);
	} finally {
		if (omp.exitCode === null) omp.kill();
		await omp.exited;
		fs.rmSync(cwd, { recursive: true, force: true });
	}
}

try {
	checkLink();
	await checkOmp();
} catch (error) {
	if (!(error instanceof CheckFailed)) throw error;
	console.error(`✗ ${error.message}`);
	process.exit(1);
}
