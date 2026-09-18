// Which venue the app is pointed at, and how it moves between them (interface 1 README, choosing an environment).
//
// The app is built knowing one address and asks that venue where else it may go. Everyone is told about the live
// venue; one of the venue's own people is also told which test environments exist, so a developer can point the app
// in their hand at the environment their branch is running in.
//
// **A session belongs to one environment and never leaves it.** The environments keep separate identity services and
// separate databases, so a member of the live venue is nobody in a test one. That is why what is kept on the device
// records *which* venue the session came from: restoring one against the wrong venue would not merely fail, it would
// send a live credential to a venue running a branch nobody has reviewed. Moving between environments therefore
// signs out — there is nothing to carry across, by design (decision G-20).

import type { Environment } from "./types";

/** Where the app asks in the first place. The only address compiled into it; everything else it is told. */
export const BOOTSTRAP_URL = process.env.EXPO_PUBLIC_VENUE_URL ?? "http://127.0.0.1:8080";

/** The venue to use when nothing has been chosen and nothing can be asked — the address the app was built with. */
export const BUILT_IN: Environment = {
  id: "built-in",
  label: "The venue this app was built for",
  url: BOOTSTRAP_URL,
  // Not claimed as live: only a venue that says so of itself is, and this is an assumption rather than an answer.
  live: false,
};

/** What is kept on the device: the session, and the venue it belongs to. */
export type Kept = {
  environment: Environment;
  member: unknown;
};

/** Reads what was kept, tolerating anything that is not the shape it should be — an old version's format, a half
 * written file — by treating it as nothing kept at all, which costs a sign-in and never a wrong venue. */
export function readKept(text: string | null): Kept | null {
  if (!text) return null;
  try {
    const parsed: unknown = JSON.parse(text);
    if (!parsed || typeof parsed !== "object") return null;
    const { environment, member } = parsed as Partial<Kept>;
    if (!environment || typeof environment !== "object") return null;
    const { id, url } = environment as Partial<Environment>;
    if (typeof id !== "string" || typeof url !== "string" || !member) return null;
    return { environment: environment as Environment, member };
  } catch {
    return null;
  }
}

/** The one to fall back to out of a list the venue gave: the live one, or the first, or the built-in address when
 * the list is empty. An app is never left with nowhere to point. */
export function fallback(environments: Environment[]): Environment {
  return environments.find((e) => e.live) ?? environments[0] ?? BUILT_IN;
}

/** Whether `kept` names a venue that is still on offer. A preview environment is torn down when its pull request is
 * done with, so a session kept against one outlives it; the app then goes back to the live venue rather than
 * retrying an address that no longer answers. */
export function stillOffered(kept: Kept, environments: Environment[]): boolean {
  return environments.some((e) => e.id === kept.environment.id && e.url === kept.environment.url);
}
