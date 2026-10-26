/**
 * thickAPI thin lane — provider `subs` (SPEC §19.10).
 *
 * `~/.pi/agent/models.json` → `providers.subs` is a plain `openai-completions` provider on
 * `http://127.0.0.1:8787/v1` (router.yaml `listen`) written by `thickapi sync-pi`. Pi keeps its own system prompt,
 * tools, skills and compaction; thickAPI is only the model.
 *
 * This extension re-registers `subs` with the same rows and a thin `streamSimple` wrapper:
 *   - pings `/ready` on `session_start` and `/model`, shows the selected row's path;
 *   - on ECONNREFUSED spawns an absolute sidecar/`THICKAPI_EXE` `serve` once (never a bare
 *     name; refuses a `\\target\\` exe without sidecar `allow_debug`) and rewrites the error;
 *   - rewrites 401 to "stale ta_ key: run `thickapi sync-pi`" (daemon re-reads keys.yaml when mtime changes);
 *   - status bar is two segments (`thickapi` path, `thickapi-quota` quota); one notify per path flip;
 *   - reads `x-thickapi-path` / `x-thickapi-rate-limit` in `after_provider_response`.
 *
 * Load after `subscription.ts` (settings.json order) so its `unregisterProvider` calls do not
 * remove this provider.
 */
import { existsSync, mkdirSync, openSync, readFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";
// Pi >= 0.85 moved the per-API lazy wrappers to the compat entrypoint.
import { openAICompletionsApi } from "@earendil-works/pi-ai/compat";
import type {
	Api,
	AssistantMessage,
	Context,
	Model,
	SimpleStreamOptions,
} from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { daemonRoot, rewriteError } from "./thickapi-subs-error.ts";
import { classifySpawnLog, planSpawn, quoteCli, readLogTail, type Sidecar } from "./thickapi-subs-spawn.ts";
import { fetchReady as fetchReadyBounded, type ReadyResult } from "./thickapi-subs-ready.ts";
import {
	parsePathHeader,
	pathFlipped,
	pathSegment,
	quotaSegment,
	vendorFromModelId,
	type PathHeader,
} from "./thickapi-subs-path.ts";
import { SEAT_CWD_HEADER, seatCwdHeaders } from "./thickapi-subs-cwd.ts";

export { parseEnvelope, rewriteError, rewriteStreamSimpleError, messageFromSseErrorFrame } from "./thickapi-subs-error.ts";
export { classifySpawnLog, isUnderTarget, planSpawn, readLogTail } from "./thickapi-subs-spawn.ts";
export {
	parsePathHeader,
	pathFlipped,
	pathSegment,
	quotaSegment,
	vendorFromModelId,
} from "./thickapi-subs-path.ts";

export { SEAT_CWD_HEADER, seatCwdHeaders } from "./thickapi-subs-cwd.ts";

const AGENT = join(homedir(), ".pi", "agent");
const PROVIDER = "subs";
const READY_TTL_MS = 60_000;

type SubsBlock = {
	api?: string;
	baseUrl?: string;
	apiKey?: string;
	compat?: Record<string, unknown>;
	models?: Array<Record<string, unknown> & { id: string; name: string }>;
};


type ReadyDoc = {
	ready?: boolean;
	message?: string;
	thin?: {
		vendors?: Record<
			string,
			{
				path?: string;
				path_reason?: string;
				oauth_mode?: string;
				cli_version?: string | null;
				token?: { present?: boolean; expired?: boolean };
				oauth?: {
					status?: string | { blocked?: { next_probe?: string } };
					last_status?: number | null;
					last_rejection?: string | null;
					probe?: { next_at?: string | null };
				};
			}
		>;
	};
};

function loadJson<T>(path: string): T | undefined {
	try {
		return JSON.parse(readFileSync(path, "utf8")) as T;
	} catch {
		return undefined;
	}
}

function subsBlock(): SubsBlock | undefined {
	const doc = loadJson<{ providers?: { subs?: SubsBlock } }>(join(AGENT, "models.json"));
	const subs = doc?.providers?.subs;
	if (!subs || subs.api !== "openai-completions" || !subs.baseUrl || !subs.apiKey) return undefined;
	return subs;
}

function sidecar(): Sidecar {
	return loadJson<Sidecar>(join(AGENT, "thickapi-subs.json")) ?? {};
}

function fetchReady(baseUrl: string, apiKey: string): Promise<ReadyResult<ReadyDoc>> {
	return fetchReadyBounded<ReadyDoc>(baseUrl, apiKey);
}

function vendorReadyPath(doc: ReadyDoc, vendor: string | undefined): PathHeader | undefined {
	if (!vendor) return undefined;
	const v = doc.thin?.vendors?.[vendor];
	if (!v?.path) return undefined;
	const blocked = v.oauth?.status;
	const probe =
		(typeof blocked === "object" ? blocked.blocked?.next_probe : undefined) ??
		v.oauth?.probe?.next_at ??
		undefined;
	return { path: v.path, probe: probe ?? undefined };
}

let spawnedOnce = false;
let lastSpawnExe = "thickapi";
let lastSpawnNote: string | undefined;
let lastSpawnLog: string | undefined;

function autostartLogPath(side: Sidecar): string {
	return join(side.configDir || join(homedir(), ".thickapi"), "logs", "autostart.log");
}

function startDaemon(): string | undefined {
	const side = sidecar();
	const logPath = autostartLogPath(side);
	lastSpawnLog = logPath;
	lastSpawnNote = undefined;
	const plan = planSpawn(side, process.env.THICKAPI_EXE, existsSync);
	if (plan.kind === "refuse") {
		lastSpawnNote = plan.reason;
		if (plan.exe) lastSpawnExe = plan.exe;
		return undefined;
	}
	if (spawnedOnce) return undefined;
	spawnedOnce = true;
	lastSpawnExe = plan.exe;
	try {
		mkdirSync(dirname(logPath), { recursive: true });
		const logFd = openSync(logPath, "a");
		const child = spawn(plan.exe, plan.args, {
			detached: true,
			stdio: ["ignore", logFd, logFd],
			windowsHide: true,
		});
		child.unref();
		return plan.exe;
	} catch (err) {
		lastSpawnNote = `spawn failed: ${plan.exe}: ${err instanceof Error ? err.message : String(err)}`;
		return undefined;
	}
}

async function ensureDaemon(
	baseUrl: string,
	apiKey: string,
	waitMs = 10_000,
): Promise<"ok" | "started" | "down" | "unauthorized"> {
	const first = await fetchReady(baseUrl, apiKey);
	if (first === "unauthorized") return "unauthorized";
	if (first !== "down") {
		spawnedOnce = false;
		return "ok";
	}
	const exe = startDaemon();
	if (!exe) return "down";
	const deadline = Date.now() + waitMs;
	let lockHeld = false;
	while (Date.now() < deadline) {
		await new Promise((r) => setTimeout(r, 500));
		const probe = await fetchReady(baseUrl, apiKey);
		if (probe === "unauthorized") return "unauthorized";
		if (probe !== "down") {
			spawnedOnce = false;
			return "started";
		}
		try {
			const log = readLogTail(lastSpawnLog ?? "");
			if (classifySpawnLog(log) === "lock-held") lockHeld = true;
		} catch {
			/* log not written yet */
		}
	}
	lastSpawnNote = lockHeld
		? `lock held by another instance`
		: `spawn failed`;
	lastSpawnNote += `: ${quoteCli(exe)}`;
	return "down";
}

export default function thickapiSubs(pi: ExtensionAPI) {
	const subs = subsBlock();
	if (!subs) {
		pi.on("session_start", (_e, ctx) => {
			ctx.ui.notify("thickAPI subs lane hidden: run `thickapi sync-pi` (needs `thickapi serve` up).", "warning");
		});
		return;
	}
	const baseUrl = subs.baseUrl!;
	const apiKey = process.env.THICKAPI_API_KEY?.trim() || subs.apiKey!;
	const api = openAICompletionsApi();
	let readyUntil = 0;
	let lastRateLimit: string | undefined;
	let lastRetryAfter: number | undefined;
	let lastPathHeader: string | undefined;
	let lastPathSeg: string | undefined;
	let lastQuotaSeg: string | undefined;
	// Session cwd Pi already has (`ExtensionContext.cwd`). One stream must not change it for the next.
	let sessionCwd: string | undefined;
	const noteCwd = (cwd: string | undefined) => {
		if (cwd) sessionCwd = cwd;
	};

	type StatusUi = {
		notify: (m: string, t?: "info" | "warning" | "error") => void;
		setStatus: (k: string, t: string | undefined) => void;
	};

	const setPathStatus = (ctx: { ui: StatusUi }, seg: string, notifyFlip: boolean) => {
		if (notifyFlip && pathFlipped(lastPathSeg, seg)) {
			ctx.ui.notify(`thickAPI: ${seg}`, "info");
		}
		lastPathSeg = seg;
		ctx.ui.setStatus("thickapi", `thickAPI ${seg}`);
	};
	const setQuotaStatus = (ctx: { ui: StatusUi }, seg: string | undefined) => {
		lastQuotaSeg = seg;
		ctx.ui.setStatus("thickapi-quota", seg ? `thickAPI ${seg}` : undefined);
	};

	const streamSimple = (model: Model<Api>, context: Context, options?: SimpleStreamOptions) => {
		const out = createAssistantMessageEventStream();
		(async () => {
			let startedNow = false;
			if (Date.now() > readyUntil) {
				const state = await ensureDaemon(baseUrl, apiKey);
				if (state === "ok" || state === "started") readyUntil = Date.now() + READY_TTL_MS;
				startedNow = state === "started";
			}
			const inner = api.streamSimple(
				{ ...model, api: "openai-completions" as Api, baseUrl },
				context,
				{
					...options,
					apiKey: options?.apiKey || apiKey,
					headers: seatCwdHeaders(options?.headers, sessionCwd),
					onResponse: async (response, m) => {
						const rl = response.headers["x-thickapi-rate-limit"];
						if (rl) lastRateLimit = rl;
						const ph = response.headers["x-thickapi-path"];
						if (ph) lastPathHeader = ph;
						const ra = Number(response.headers["retry-after"]);
						lastRetryAfter = Number.isFinite(ra) && ra > 0 ? ra : undefined;
						if (response.status >= 500) readyUntil = 0;
						await options?.onResponse?.(response, m);
					},
				},
			);
			let final: AssistantMessage | undefined;
			for await (const event of inner) {
				if (event.type === "error") {
					readyUntil = 0;
					const err = event.error;
					const rewritten = rewriteError(
						err.errorMessage,
						baseUrl,
						startedNow,
						lastSpawnExe || sidecar().exe || "thickapi",
						{ spawnNote: lastSpawnNote, logPath: lastSpawnLog },
					);
					const patched = { ...err, errorMessage: rewritten };
					out.push({ ...event, error: patched });
					final = patched;
					continue;
				}
				out.push(event);
				if (event.type === "done") final = event.message;
			}
			out.end(final);
		})().catch((error) => {
			readyUntil = 0;
			const message = rewriteError(
				error instanceof Error ? error.message : String(error),
				baseUrl,
				false,
				lastSpawnExe || sidecar().exe || "thickapi",
				{ spawnNote: lastSpawnNote, logPath: lastSpawnLog },
			);
			const failed: AssistantMessage = {
				role: "assistant",
				content: [],
				api: "openai-completions" as Api,
				provider: PROVIDER,
				model: model.id,
				usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
				stopReason: "error",
				errorMessage: message,
				timestamp: Date.now(),
			} as AssistantMessage;
			out.push({ type: "error", reason: "error", error: failed });
			out.end(failed);
		});
		return out;
	};

	pi.registerProvider(PROVIDER, {
		name: "thickAPI subs (thin)",
		api: "openai-completions",
		baseUrl,
		apiKey,
		models: (subs.models ?? []) as never,
		streamSimple,
	});

	const report = async (
		ctx: { ui: StatusUi; model?: { id?: string; provider?: string } },
		modelId?: string,
	) => {
		const doc = await fetchReady(baseUrl, apiKey);
		if (doc === "down") {
			ctx.ui.setStatus("thickapi", "thickAPI: down");
			ctx.ui.setStatus("thickapi-quota", undefined);
			ctx.ui.notify(`thickAPI daemon not reachable at ${daemonRoot(baseUrl)}; first turn auto-starts it.`, "warning");
			return;
		}
		if (doc === "unauthorized") {
			ctx.ui.setStatus("thickapi", "thickAPI: stale key");
			ctx.ui.setStatus("thickapi-quota", undefined);
			ctx.ui.notify("thickAPI key rejected: run `thickapi sync-pi`, then restart Pi.", "error");
			return;
		}
		if (doc === "error") {
			ctx.ui.setStatus("thickapi", "thickAPI: error");
			return;
		}
		const vendor = vendorFromModelId(modelId ?? ctx.model?.id) ?? "claude";
		const ready = vendorReadyPath(doc, vendor);
		setPathStatus(ctx, pathSegment(vendor, ready, ready?.path), false);
	};

	pi.on("session_start", async (_e, ctx) => {
		noteCwd(ctx.cwd);
		if (ctx.model?.provider === PROVIDER) await report(ctx, ctx.model?.id);
	});
	pi.on("turn_start", (_e, ctx) => {
		noteCwd(ctx.cwd);
	});
	// Fires before this provider's `streamSimple`, with the cwd of this request.
	pi.on("before_provider_headers", (e, ctx) => {
		if (ctx.model?.provider !== PROVIDER) return;
		noteCwd(ctx.cwd);
		if (ctx.cwd) e.headers[SEAT_CWD_HEADER] = ctx.cwd;
	});
	pi.on("model_select", async (e, ctx) => {
		if (e.model?.provider === PROVIDER) await report(ctx, e.model?.id);
		else {
			lastPathSeg = undefined;
			lastQuotaSeg = undefined;
			ctx.ui.setStatus("thickapi", undefined);
			ctx.ui.setStatus("thickapi-quota", undefined);
		}
	});
	pi.on("after_provider_response", (e, ctx) => {
		if (ctx.model?.provider !== PROVIDER) return;
		const vendor = vendorFromModelId(ctx.model?.id) ?? "claude";
		const rawPath = e.headers?.["x-thickapi-path"] ?? lastPathHeader;
		const parsed = parsePathHeader(rawPath);
		if (parsed) {
			setPathStatus(ctx, pathSegment(vendor, parsed), true);
		}
		const rl = e.headers?.["x-thickapi-rate-limit"] ?? lastRateLimit;
		const raHeader = Number(e.headers?.["retry-after"]);
		const retry = Number.isFinite(raHeader) && raHeader > 0 ? raHeader : lastRetryAfter;
		const quota = quotaSegment(rl, retry);
		if (quota !== undefined) {
			setQuotaStatus(ctx, quota);
			const pct = Number(quota.match(/quota (\d+)%/)?.[1]);
			if (Number.isFinite(pct) && pct >= 80) {
				ctx.ui.notify(`thickAPI: vendor quota ${pct}% used`, "warning");
			}
		}
	});
}
