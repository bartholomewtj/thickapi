/** Pi error rewrite (SPEC §12, §19.4). No Pi SDK imports so `bun test` can load this file alone. */

export type ErrorEnvelope = {
	message?: string;
	type?: string;
	code?: string;
	vendor?: string;
	hint?: string;
	/**
	 * Seconds; mirrors `Retry-After`. On the HTTP path the envelope carries this field; on a
	 * mid-stream error Pi surfaces `message` alone, so the daemon folds it into that string.
	 */
	retry_after?: number;
	/** Mirrors `x-should-retry`. Folded into mid-stream `message`, as above. */
	should_retry?: boolean;
};

export function daemonRoot(baseUrl: string): string {
	return baseUrl.replace(/\/v1\/?$/, "").replace(/\/$/, "");
}

export function parseEnvelope(text: string): ErrorEnvelope | undefined {
	const start = text.indexOf("{");
	if (start < 0) return undefined;
	try {
		const doc = JSON.parse(text.slice(start)) as { error?: ErrorEnvelope } & ErrorEnvelope;
		const err = doc.error ?? doc;
		if (err && typeof err === "object" && (err.message || err.type || err.code)) {
			return err;
		}
	} catch {
		return undefined;
	}
	return undefined;
}

/** The daemon's trailing ` [code=<code>]` on every thin error message (SPEC §19.4, #136). */
const CODE_MARKER = /\s*\[code=([a-z0-9_]+)\]/i;

export function codeMarker(text: string | undefined): string | undefined {
	return text?.match(CODE_MARKER)?.[1]?.toLowerCase();
}

function withoutCodeMarker(text: string): string {
	return text.replace(CODE_MARKER, "");
}

/** Pi records the rewritten text; the marker goes back on the end so draftr can still read it. */
function tagged(text: string, code: string | undefined): string {
	return code ? `${text} [code=${code}]` : text;
}

/** A folded `(retry_after=401s, should_retry=true)` must not read as an HTTP 401. */
function withoutRetryHints(text: string): string {
	return text.replace(/\bretry_after=\d+s\b/gi, "");
}

const STALE_KEY =
	"stale ta_ key for thickAPI: run `thickapi sync-pi`, then restart Pi (the daemon picks the key up itself)";
const LACKS_CHAT_WRITE = "thickAPI key lacks chat:write: run `thickapi sync-pi` (mints a chat:write key)";

function withoutStatusDigits(text: string): string {
	return text.replace(/\b[45]\d\d\b/g, "").replace(/\s{2,}/g, " ").trim();
}

function renderHint(message: string, hint?: string): string {
	return hint ? `${message} (${hint})` : message;
}

function quoteCli(exe: string): string {
	return /[\s\\/]/.test(exe) ? `"${exe}"` : exe;
}

export function rewriteError(
	message: string | undefined,
	baseUrl: string,
	startedNow: boolean,
	exe = "thickapi",
	extra?: { spawnNote?: string; logPath?: string },
): string {
	const text = message ?? "unknown error";
	if (/ECONNREFUSED|fetch failed|ENOTFOUND|ECONNRESET/i.test(text)) {
		const root = daemonRoot(baseUrl);
		const serve = `${quoteCli(exe)} serve`;
		const install = `${quoteCli(exe)} install-service`;
		if (startedNow) {
			const log = extra?.logPath ? `; log ${extra.logPath}` : "";
			return `thickAPI daemon not running at ${root} (started ${quoteCli(exe)}, retry${log})`;
		}
		if (extra?.spawnNote) {
			const log = extra?.logPath ? ` (log ${extra.logPath})` : "";
			return `thickAPI daemon not running at ${root}: ${extra.spawnNote}${log}`;
		}
		return `thickAPI daemon not running at ${root}: run \`${serve}\` (or \`${install}\`)`;
	}
	const env = parseEnvelope(text);
	// The class comes from the envelope `code` or the daemon's `[code=…]` marker, never from
	// status digits in free text (#128: `retry_after=401s` is not a stale key).
	const code = (env?.code || codeMarker(env?.message) || codeMarker(text))?.toLowerCase();
	const vendorFault = !!env?.vendor || /^vendor\s/i.test(text);
	if (env) {
		const message = withoutCodeMarker(env.message || text);
		if (code === "builtin_loop" || code === "text_tool_call") {
			return tagged(renderHint(message, env.hint), code);
		}
		if (env.type === "vendor_drift" || code === "vendor_drift") {
			const vendor = env.vendor || "vendor";
			const msg = withoutStatusDigits(message);
			return tagged(`thickAPI: ${vendor} wire changed — ${msg}. Run ${exe} doctor`, code);
		}
		if ((env.type === "authentication_error" || code === "authentication_error") && !env.vendor) {
			return tagged(STALE_KEY, code);
		}
		if (code === "permission_error" && !env.vendor) {
			return tagged(LACKS_CHAT_WRITE, code);
		}
		return tagged(renderHint(message, env.hint), code);
	}
	if (code) {
		// A mid-stream frame: Pi hands back the bare message, marker included.
		const message = withoutCodeMarker(text);
		if (code === "builtin_loop" || code === "text_tool_call") {
			return tagged(message, code);
		}
		if (code === "vendor_drift") {
			return tagged(`thickAPI: vendor wire changed — ${withoutStatusDigits(message)}. Run ${exe} doctor`, code);
		}
		if (code === "authentication_error" && !vendorFault) return tagged(STALE_KEY, code);
		if (code === "permission_error" && !vendorFault) return tagged(LACKS_CHAT_WRITE, code);
		return text;
	}
	// An older daemon without the marker. Mid-stream vendor 401/403 is prefixed `vendor ` so it
	// does not look like a stale ta_ key.
	if (vendorFault) {
		return text;
	}
	const plain = withoutRetryHints(text);
	if (/\b401\b|authentication_error|unknown api key|invalid api key/i.test(plain)) {
		return STALE_KEY;
	}
	if (/\b403\b|permission_error|chat:write/i.test(plain)) {
		return LACKS_CHAT_WRITE;
	}
	return text;
}

/**
 * What `streamSimple` sees from a mid-stream error chunk: openai-node throws on `data.error`
 * and Pi hands back `error.message` alone (SPEC §19.4).
 */
export function messageFromSseErrorFrame(sse: string): string | undefined {
	for (const part of sse.split("\n\n")) {
		const line = part
			.split("\n")
			.find((l) => l.startsWith("data:"));
		if (!line) continue;
		const data = line.slice("data:".length).trim();
		if (!data || data === "[DONE]") continue;
		try {
			const doc = JSON.parse(data) as { error?: ErrorEnvelope };
			if (doc.error?.message) return doc.error.message;
		} catch {
			/* not a JSON frame */
		}
	}
	return undefined;
}

/** The `streamSimple` wrapper: take the SSE error frame's message and rewrite it. */
export function rewriteStreamSimpleError(
	sse: string,
	baseUrl: string,
	startedNow: boolean,
	exe = "thickapi",
): string {
	return rewriteError(messageFromSseErrorFrame(sse), baseUrl, startedNow, exe);
}
