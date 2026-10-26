/** Daemon auto-start planning (SPEC §12, §19.10). No Pi SDK imports so `bun test` can load this alone. */

import { closeSync, fstatSync, openSync, readSync } from "node:fs";
import { isAbsolute } from "node:path";

export type Sidecar = {
	exe?: string;
	configDir?: string;
	baseUrl?: string;
	allow_debug?: boolean;
};

export type SpawnPlan =
	| { kind: "spawn"; exe: string; args: string[] }
	| { kind: "refuse"; reason: string; exe?: string };

export function isUnderTarget(exe: string): boolean {
	return exe
		.replace(/\\/g, "/")
		.split("/")
		.some((part) => part.toLowerCase() === "target");
}

export function quoteCli(exe: string): string {
	return /[\s\\/]/.test(exe) ? `"${exe}"` : exe;
}

export function planSpawn(
	sidecar: Sidecar,
	envExe: string | undefined,
	exists: (path: string) => boolean,
): SpawnPlan {
	const candidate = envExe?.trim() || sidecar.exe?.trim();
	if (!candidate) {
		return { kind: "refuse", reason: "no sidecar exe and THICKAPI_EXE unset" };
	}
	if (!isAbsolute(candidate)) {
		return { kind: "refuse", reason: `refusing bare-name spawn: ${candidate}`, exe: candidate };
	}
	if (!exists(candidate)) {
		return { kind: "refuse", reason: `exe not found: ${candidate}`, exe: candidate };
	}
	if (isUnderTarget(candidate) && sidecar.allow_debug !== true) {
		return {
			kind: "refuse",
			reason: `refusing \\target\\ exe without allow_debug: ${candidate}`,
			exe: candidate,
		};
	}
	const args = ["serve"];
	if (sidecar.configDir) args.push("--config", sidecar.configDir);
	return { kind: "spawn", exe: candidate, args };
}

export function classifySpawnLog(text: string): "lock-held" | "spawn-failed" {
	return /already running|AlreadyHeld/i.test(text) ? "lock-held" : "spawn-failed";
}

/** Last ~8KiB of a spawn log. `ensureDaemon` must not re-read the whole file every 500 ms. */
export const LOG_TAIL_BYTES = 8 * 1024;

export function readLogTail(path: string, maxBytes = LOG_TAIL_BYTES): string {
	const fd = openSync(path, "r");
	try {
		const { size } = fstatSync(fd);
		if (size <= 0) return "";
		const start = Math.max(0, size - maxBytes);
		const len = size - start;
		const buf = Buffer.alloc(len);
		const n = readSync(fd, buf, 0, len, start);
		return buf.subarray(0, n).toString("utf8");
	} finally {
		closeSync(fd);
	}
}
