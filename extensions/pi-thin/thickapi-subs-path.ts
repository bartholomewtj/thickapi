/** Path/quota status helpers for the Pi thin extension (SPEC §12, §19.15 R4). */

export type PathHeader = { path: string; reason?: string; probe?: string };

export function parsePathHeader(raw: string | undefined): PathHeader | undefined {
	if (!raw) return undefined;
	const parts = raw.split(";").map((p) => p.trim()).filter(Boolean);
	const path = parts[0];
	if (!path) return undefined;
	const out: PathHeader = { path };
	for (const p of parts.slice(1)) {
		const eq = p.indexOf("=");
		if (eq < 0) continue;
		const k = p.slice(0, eq);
		const v = p.slice(eq + 1);
		if (k === "reason") out.reason = v;
		if (k === "probe") out.probe = v;
	}
	return out;
}

export function vendorFromModelId(id: string | undefined): string | undefined {
	if (!id) return undefined;
	return id.split("/")[0] || undefined;
}

export function formatProbeClock(iso: string): string {
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return iso;
	return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

export function pathSegment(
	vendor: string,
	header: PathHeader | undefined,
	readyPath?: string,
): string {
	const path = header?.path ?? readyPath ?? "oauth";
	if (vendor !== "claude") return vendor;
	if (path === "bridge") {
		const clock = header?.probe ? formatProbeClock(header.probe) : undefined;
		return clock ? `claude via bridge (probe ${clock})` : "claude via bridge";
	}
	return "claude direct";
}

export function quotaSegment(
	rateLimit: string | undefined,
	retryAfterSeconds?: number,
): string | undefined {
	const retryText =
		retryAfterSeconds && retryAfterSeconds > 0 ? ` retry in ${retryAfterSeconds}s` : "";
	if (!rateLimit) return retryText ? retryText.trim() : undefined;
	const [util, reset] = rateLimit.split(";");
	const pct = Math.round(Number(util) * 100);
	if (!Number.isFinite(pct)) return retryText ? retryText.trim() : undefined;
	// At or below half the window the seat is not short of quota, so the footer stays quiet.
	if (pct <= 50) return retryText ? retryText.trim() : undefined;
	const resetText = reset?.startsWith("reset=") ? ` (resets ${reset.slice(6)})` : "";
	return `quota ${pct}%${resetText}${retryText}`;
}

export function pathFlipped(previous: string | undefined, next: string): boolean {
	return previous !== undefined && previous !== next;
}
