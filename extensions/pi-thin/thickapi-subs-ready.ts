/**
 * `/ready` for the subs lane, bounded (thickAPI #124, audit R4). `streamSimple` awaits this
 * before a request once the ready TTL has run out, so a daemon that accepts and never answers
 * must not hang every Pi turn. A `/ready` that times out is checked against `/health`: a live
 * daemon that is only slow is not "down" (which would try to spawn a second one).
 */
import { daemonRoot } from "./thickapi-subs-error.ts";

export const READY_TIMEOUT_MS = 3_000;

export type ReadyResult<T> = T | "down" | "unauthorized" | "error";

export async function fetchReady<T>(
	baseUrl: string,
	apiKey: string,
	timeoutMs = READY_TIMEOUT_MS,
): Promise<ReadyResult<T>> {
	const root = daemonRoot(baseUrl);
	try {
		const response = await fetch(`${root}/ready`, {
			headers: { Authorization: `Bearer ${apiKey}` },
			signal: AbortSignal.timeout(timeoutMs),
		});
		if (response.status === 401 || response.status === 403) return "unauthorized";
		if (!response.ok) return "error";
		return (await response.json()) as T;
	} catch {
		return (await alive(root, timeoutMs)) ? "error" : "down";
	}
}

/** `/health` answers without auth and without touching vendors: the liveness check. */
export async function alive(root: string, timeoutMs = READY_TIMEOUT_MS): Promise<boolean> {
	try {
		const response = await fetch(`${root}/health`, { signal: AbortSignal.timeout(timeoutMs) });
		return response.ok;
	} catch {
		return false;
	}
}
