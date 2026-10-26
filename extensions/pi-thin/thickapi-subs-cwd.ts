/**
 * Per-request seat cwd on the subs stream (SPEC §19.8, #205).
 * Pi's `SimpleStreamOptions.headers` is merged onto the outgoing request.
 */
export const SEAT_CWD_HEADER = "x-thickapi-cwd";

export function seatCwdHeaders(
	headers: Record<string, string | null> | undefined,
	cwd: string | undefined,
): Record<string, string | null> | undefined {
	if (!cwd) return headers;
	return { ...headers, [SEAT_CWD_HEADER]: cwd };
}
