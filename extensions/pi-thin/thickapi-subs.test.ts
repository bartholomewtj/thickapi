import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rewriteError } from "./thickapi-subs-error.ts";
import { classifySpawnLog, isUnderTarget, planSpawn, readLogTail, LOG_TAIL_BYTES } from "./thickapi-subs-spawn.ts";
import {
	parsePathHeader,
	pathFlipped,
	pathSegment,
	quotaSegment,
	vendorFromModelId,
} from "./thickapi-subs-path.ts";

const BASE = "http://127.0.0.1:18787/v1";

describe("rewriteError", () => {
	test("daemon 401 authentication_error without vendor → sync-pi hint", () => {
		const body = JSON.stringify({
			error: { message: "unknown api key", type: "authentication_error", code: "authentication_error" },
		});
		expect(rewriteError(body, BASE, false)).toContain("stale ta_ key");
		expect(rewriteError(body, BASE, false)).toContain("sync-pi");
	});

	test("vendor 401 with vendor field is verbatim message and hint", () => {
		const body = JSON.stringify({
			error: {
				message: "grok token expired",
				type: "vendor_auth",
				code: "vendor_login_required",
				vendor: "grok",
				hint: "grok login",
			},
		});
		expect(rewriteError(body, BASE, false)).toBe("grok token expired (grok login) [code=vendor_login_required]");
		expect(rewriteError(body, BASE, false)).not.toContain("sync-pi");
	});

	test("vendor_drift has no status digits and names doctor", () => {
		const body = JSON.stringify({
			error: {
				message: "cursor 502: frame field 7 moved",
				type: "vendor_drift",
				code: "vendor_drift",
				vendor: "cursor",
				hint: "thickapi doctor",
			},
		});
		const out = rewriteError(body, BASE, false, "C:\\tools\\thickapi.exe");
		expect(out.startsWith("thickAPI: cursor wire changed — ")).toBe(true);
		expect(out.endsWith("Run C:\\tools\\thickapi.exe doctor [code=vendor_drift]")).toBe(true);
		expect(out).not.toMatch(/\b[45]\d\d\b/);
		expect(out).not.toContain("sync-pi");
	});

	test("builtin_loop envelope keeps the daemon hint and is not wire changed", () => {
		const body = JSON.stringify({
			error: {
				message: "same tool call answered the same way",
				type: "vendor_drift",
				code: "builtin_loop",
				vendor: "cursor",
				hint: "send a different tool call",
			},
		});
		const out = rewriteError(body, BASE, false);
		expect(out).toBe(
			"same tool call answered the same way (send a different tool call) [code=builtin_loop]",
		);
		expect(out).not.toContain("wire changed");
		expect(out).not.toContain("doctor");
	});

	test("text_tool_call envelope keeps the daemon hint and is not wire changed", () => {
		const body = JSON.stringify({
			error: {
				message: "tool_call block did not parse",
				type: "vendor_drift",
				code: "text_tool_call",
				vendor: "cursor",
				hint: "use the tool-call interface",
			},
		});
		const out = rewriteError(body, BASE, false);
		expect(out).toBe("tool_call block did not parse (use the tool-call interface) [code=text_tool_call]");
		expect(out).not.toContain("wire changed");
		expect(out).not.toContain("doctor");
	});

	test("mid-stream builtin_loop marker is not rewritten as wire changed", () => {
		const msg = "same tool call answered the same way [code=builtin_loop]";
		const out = rewriteError(msg, BASE, false);
		expect(out).toBe(msg);
		expect(out).not.toContain("wire changed");
		expect(out).not.toContain("doctor");
		expect(out).toContain("[code=builtin_loop]");
	});
});

describe("path visibility", () => {
	test("parsePathHeader reads path, reason, probe", () => {
		expect(parsePathHeader("oauth")).toEqual({ path: "oauth" });
		expect(parsePathHeader("bridge;reason=token_expired")).toEqual({
			path: "bridge",
			reason: "token_expired",
		});
		expect(parsePathHeader("bridge;reason=blocked;probe=2026-09-19T09:43:00Z")).toEqual({
			path: "bridge",
			reason: "blocked",
			probe: "2026-09-19T09:43:00Z",
		});
	});

	test("pathSegment is the selected vendor only", () => {
		expect(pathSegment("claude", { path: "oauth" })).toBe("claude direct");
		expect(pathSegment("claude", { path: "bridge" })).toBe("claude via bridge");
		expect(pathSegment("claude", { path: "bridge", probe: "2026-09-19T09:43:00Z" })).toMatch(
			/^claude via bridge \(probe \d{2}:\d{2}\)$/,
		);
		expect(pathSegment("grok", { path: "oauth" })).toBe("grok");
		expect(vendorFromModelId("claude/sonnet")).toBe("claude");
	});

	test("quota stays hidden at or below 50% and does not replace the path segment", () => {
		expect(quotaSegment("0")).toBeUndefined();
		expect(quotaSegment("0.25")).toBeUndefined();
		expect(quotaSegment("0.5")).toBeUndefined();
		expect(quotaSegment("0.25", 12)).toBe("retry in 12s");
		expect(quotaSegment("0.51")).toBe("quota 51%");
		expect(quotaSegment("0.8;reset=noon", 12)).toBe("quota 80% (resets noon) retry in 12s");
		const path = pathSegment("claude", { path: "oauth" });
		const quota = quotaSegment("0.9");
		expect(path).toBe("claude direct");
		expect(quota).toBe("quota 90%");
		expect(path).not.toContain("quota");
	});

	test("notify once per flip, not the first set", () => {
		expect(pathFlipped(undefined, "claude direct")).toBe(false);
		expect(pathFlipped("claude direct", "claude direct")).toBe(false);
		expect(pathFlipped("claude direct", "claude via bridge")).toBe(true);
	});
});

describe("daemon spawn plan", () => {
	const exists = (p: string) => p === "C:\\tools\\thickapi.exe" || p === "C:\\repo\\target\\debug\\thickapi.exe";

	test("bare-name spawn is impossible", () => {
		expect(planSpawn({}, undefined, exists).kind).toBe("refuse");
		expect(planSpawn({}, "thickapi", exists)).toEqual({
			kind: "refuse",
			reason: "refusing bare-name spawn: thickapi",
			exe: "thickapi",
		});
		expect(planSpawn({ exe: "thickapi" }, undefined, exists).kind).toBe("refuse");
	});

	test("target exe is refused without allow_debug", () => {
		expect(isUnderTarget("C:\\repo\\target\\debug\\thickapi.exe")).toBe(true);
		const refused = planSpawn(
			{ exe: "C:\\repo\\target\\debug\\thickapi.exe" },
			undefined,
			exists,
		);
		expect(refused.kind).toBe("refuse");
		if (refused.kind === "refuse") {
			expect(refused.reason).toContain("allow_debug");
		}
		const allowed = planSpawn(
			{ exe: "C:\\repo\\target\\debug\\thickapi.exe", allow_debug: true, configDir: "C:\\cfg" },
			undefined,
			exists,
		);
		expect(allowed).toEqual({
			kind: "spawn",
			exe: "C:\\repo\\target\\debug\\thickapi.exe",
			args: ["serve", "--config", "C:\\cfg"],
		});
	});

	test("absolute sidecar or THICKAPI_EXE is spawned", () => {
		expect(planSpawn({ exe: "C:\\tools\\thickapi.exe" }, undefined, exists)).toEqual({
			kind: "spawn",
			exe: "C:\\tools\\thickapi.exe",
			args: ["serve"],
		});
		expect(planSpawn({}, "C:\\tools\\thickapi.exe", exists).kind).toBe("spawn");
	});

	test("lock-held log is distinct from a spawn failure", () => {
		expect(classifySpawnLog("another thickapi instance is already running (lock: x)")).toBe(
			"lock-held",
		);
		expect(classifySpawnLog("Error: bind 127.0.0.1:18787")).toBe("spawn-failed");
	});

	test("readLogTail is the last 8KiB so classifySpawnLog still sees a recent lock", () => {
		const dir = mkdtempSync(join(tmpdir(), "thickapi-log-"));
		const path = join(dir, "autostart.log");
		try {
			const noise = "spawn noise line\n".repeat(2000);
			expect(Buffer.byteLength(noise, "utf8")).toBeGreaterThan(LOG_TAIL_BYTES);
			writeFileSync(path, noise + "Error: bind 127.0.0.1:19787\n");
			const tail = readLogTail(path);
			expect(Buffer.byteLength(tail, "utf8")).toBeLessThanOrEqual(LOG_TAIL_BYTES + 16);
			expect(classifySpawnLog(tail)).toBe("spawn-failed");
			writeFileSync(path, noise + "another thickapi instance is already running\n");
			expect(classifySpawnLog(readLogTail(path))).toBe("lock-held");
			writeFileSync(path, "already running\n" + noise);
			expect(classifySpawnLog(readLogTail(path))).toBe("spawn-failed");
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});
});

describe("rewriteError daemon hints", () => {
	test("ECONNREFUSED names the exe path", () => {
		const out = rewriteError("ECONNREFUSED", BASE, false, "C:\\tools\\thickapi.exe");
		expect(out).toContain('run `"C:\\tools\\thickapi.exe" serve`');
		expect(out).toContain("install-service");
		expect(out).not.toMatch(/run `thickapi serve`/);
	});
});

describe("retry fields in the error envelope (SPEC §19.4)", () => {
	// HTTP error (before the stream opens): openai-node keeps the parsed `error` object and
	// pi-ai's `formatProviderError` renders `"<status>: <that object>"`. The envelope arrives.
	test("HTTP path: the retry fields parse and the message is shown as before", async () => {
		const { parseEnvelope } = await import("./thickapi-subs-error.ts");
		const http =
			'429: {"message":"grok 429: slow down","type":"rate_limit_error","param":null,"code":"rate_limit_error","vendor":"grok","retry_after":7,"should_retry":true}';
		const env = parseEnvelope(http);
		expect(env?.retry_after).toBe(7);
		expect(env?.should_retry).toBe(true);
		expect(rewriteError(http, BASE, false)).toBe("grok 429: slow down [code=rate_limit_error]");
	});

	// Mid-stream error frame: openai-node throws `APIError(undefined, data.error, …)`, and with
	// no status pi-ai hands back `error.message` alone. The envelope never reaches this file.
	test("mid-stream: Pi hands back the bare message, which passes through", async () => {
		const { parseEnvelope } = await import("./thickapi-subs-error.ts");
		const bare = "grok 429: slow down";
		expect(parseEnvelope(bare)).toBeUndefined();
		expect(rewriteError(bare, BASE, false)).toBe(bare);
	});
});

describe("fake SSE through streamSimple (SPEC §19.4)", () => {
	test("mid-stream vendor 401 is not rewritten as a stale ta_ key", async () => {
		const { rewriteStreamSimpleError } = await import("./thickapi-subs-error.ts");
		const sse =
			"data: " +
			JSON.stringify({
				error: {
					message: "vendor 401: grok 401: expired",
					type: "vendor_auth",
					code: "vendor_login_required",
					vendor: "grok",
				},
			}) +
			"\n\n";
		const out = rewriteStreamSimpleError(sse, BASE, false);
		expect(out).toContain("vendor 401:");
		expect(out).not.toContain("stale ta_ key");
		expect(out).not.toContain("sync-pi");
	});

	test("mid-stream retry hints in the message survive rewrite", async () => {
		const { rewriteStreamSimpleError, messageFromSseErrorFrame } = await import(
			"./thickapi-subs-error.ts"
		);
		const sse =
			"data: " +
			JSON.stringify({
				error: {
					message: "slow down (retry_after=7s, should_retry=true)",
					type: "rate_limit_error",
					code: "rate_limit_error",
					vendor: "grok",
					retry_after: 7,
					should_retry: true,
				},
			}) +
			"\n\n";
		expect(messageFromSseErrorFrame(sse)).toBe("slow down (retry_after=7s, should_retry=true)");
		expect(rewriteStreamSimpleError(sse, BASE, false)).toContain("retry_after=7s");
		expect(rewriteStreamSimpleError(sse, BASE, false)).toContain("should_retry=true");
	});

	test("a bare mid-stream 401 without the vendor prefix is still a stale key", async () => {
		const { rewriteStreamSimpleError } = await import("./thickapi-subs-error.ts");
		const sse = 'data: {"error":{"message":"401 unknown api key","type":"authentication_error"}}\n\n';
		expect(rewriteStreamSimpleError(sse, BASE, false)).toContain("stale ta_ key");
	});
});

describe("[code=] marker (thickapi#136, #128)", () => {
	test("a folded retry_after=401s on a 429 is not a stale key (with the marker)", () => {
		const msg = "grok 429: slow down (retry_after=401s, should_retry=true) [code=rate_limit_error]";
		const out = rewriteError(msg, BASE, false);
		expect(out).toBe(msg);
		expect(out).not.toContain("stale ta_ key");
	});

	test("a folded retry_after=401s on a 429 is not a stale key (older daemon, no marker)", () => {
		const msg = "grok 429: slow down (retry_after=401s, should_retry=true)";
		expect(rewriteError(msg, BASE, false)).toBe(msg);
	});

	test("a folded retry_after=403s is not a missing chat:write scope", () => {
		const msg = "cursor 429: busy (retry_after=403s, should_retry=true)";
		expect(rewriteError(msg, BASE, false)).toBe(msg);
	});

	test("HTTP envelope: the daemon marker ends the rewritten text exactly once", () => {
		const http =
			'503: {"message":"cursor unavailable: shutting down [code=vendor_down]","type":"vendor_down","param":null,"code":"vendor_down","vendor":"cursor","hint":"thickapi doctor"}';
		const out = rewriteError(http, BASE, false);
		expect(out).toBe("cursor unavailable: shutting down (thickapi doctor) [code=vendor_down]");
		expect(out.match(/\[code=/g)?.length).toBe(1);
	});

	test("mid-stream: the marker names a stale key without any status digits", () => {
		const out = rewriteError("unknown api key [code=authentication_error]", BASE, false);
		expect(out).toContain("stale ta_ key");
		expect(out.endsWith(" [code=authentication_error]")).toBe(true);
	});

	test("mid-stream: a vendor 403 permission error is not the chat:write hint", () => {
		const msg = "vendor 403: grok 403: forbidden [code=permission_error]";
		expect(rewriteError(msg, BASE, false)).toBe(msg);
	});

	test("mid-stream drift keeps the marker after the doctor hint", () => {
		const out = rewriteError("claude 2.1.283: init moved [code=vendor_drift]", BASE, false);
		expect(out.startsWith("thickAPI: vendor wire changed — ")).toBe(true);
		expect(out.endsWith("doctor [code=vendor_drift]")).toBe(true);
	});

	test("SSE frame: the marker survives streamSimple's rewrite", async () => {
		const { rewriteStreamSimpleError } = await import("./thickapi-subs-error.ts");
		const sse =
			"data: " +
			JSON.stringify({
				error: {
					message: "cursor unavailable: Server is shutting down [code=vendor_down]",
					type: "vendor_down",
					code: "vendor_down",
					vendor: "cursor",
				},
			}) +
			"\n\n";
		expect(rewriteStreamSimpleError(sse, BASE, false)).toBe(
			"cursor unavailable: Server is shutting down [code=vendor_down]",
		);
	});
});

describe("x-thickapi-cwd on the subs stream (SPEC §19.8, #205)", () => {
	test("the outgoing request carries the session cwd", async () => {
		const { seatCwdHeaders, SEAT_CWD_HEADER } = await import("./thickapi-subs-cwd.ts");
		const seat = "C:\\seat\\repo";
		let got: string | null = null;
		const server = Bun.serve({
			port: 0,
			fetch(req) {
				got = req.headers.get(SEAT_CWD_HEADER);
				return new Response("ok");
			},
		});
		try {
			const headers = seatCwdHeaders({ authorization: "Bearer ta_x" }, seat);
			await fetch(`http://127.0.0.1:${server.port}/v1/chat/completions`, {
				method: "POST",
				headers: headers as Record<string, string>,
			});
			expect(got).toBe(seat);
		} finally {
			server.stop(true);
		}
	});

	test("no session cwd adds no header", async () => {
		const { seatCwdHeaders } = await import("./thickapi-subs-cwd.ts");
		expect(seatCwdHeaders(undefined, undefined)).toBeUndefined();
		expect(seatCwdHeaders({ authorization: "Bearer ta_x" }, undefined)).toEqual({
			authorization: "Bearer ta_x",
		});
	});
});

describe("bounded /ready (thickAPI #124)", () => {
	const hang = () => new Promise<Response>(() => {});

	test("a daemon that never answers /ready rejects within the timeout; /health says it is alive", async () => {
		const { fetchReady } = await import("./thickapi-subs-ready.ts");
		const server = Bun.serve({
			port: 0,
			fetch: (req) => (new URL(req.url).pathname === "/health" ? new Response("ok") : hang()),
		});
		try {
			const started = Date.now();
			const out = await fetchReady(`http://127.0.0.1:${server.port}/v1`, "ta_x", 300);
			expect(out).toBe("error");
			expect(Date.now() - started).toBeLessThan(2_000);
		} finally {
			server.stop(true);
		}
	});

	test("nothing answering at all is down", async () => {
		const { fetchReady } = await import("./thickapi-subs-ready.ts");
		const server = Bun.serve({ port: 0, fetch: () => hang() });
		try {
			const started = Date.now();
			const out = await fetchReady(`http://127.0.0.1:${server.port}/v1`, "ta_x", 300);
			expect(out).toBe("down");
			expect(Date.now() - started).toBeLessThan(2_000);
		} finally {
			server.stop(true);
		}
	});

	test("the default bound is 3 s", async () => {
		const { READY_TIMEOUT_MS } = await import("./thickapi-subs-ready.ts");
		expect(READY_TIMEOUT_MS).toBe(3_000);
	});
});
