/**
 * SpacePrimitives decorator that auto-manages the version-history hash protocol
 * for .md files via the generic `HttpHeaderHook` extension point on
 * `HttpSpacePrimitives`. Adds X-Parent-Hash on writes from the sync snapshot's
 * `syncedHashes` map; observes X-Content-Hash on reads/writes to update the
 * map.
 *
 * Wrapping the underlying primitives keeps the merge feature additive and lets
 * future upstream changes to HttpSpacePrimitives roll in cleanly.
 */
import type { SpacePrimitives } from "../spaces/space_primitives.ts";
import type {
  HttpHeaderHook,
  HttpSpacePrimitives,
} from "../spaces/http_space_primitives.ts";
import type { SyncSnapshot } from "../spaces/sync.ts";

function isMd(path: string): boolean {
  return path.endsWith(".md");
}

/**
 * Install a header hook on `http` that:
 *  - On `.md` writes: emits `X-Parent-Hash` from snapshot.syncedHashes[path].
 *  - On `.md` reads + writes: captures the response `X-Content-Hash` into
 *    snapshot.syncedHashes[path].
 *  - On `.md` deletes: removes any tracked hash for `path`.
 */
export function installHashTrackingHook(
  http: HttpSpacePrimitives,
  getSnapshot: () => SyncSnapshot,
): void {
  const hook: HttpHeaderHook = {
    requestHeaders(op, path) {
      if (op === "write" && isMd(path)) {
        const h = getSnapshot().syncedHashes.get(path);
        if (h) return { "X-Parent-Hash": h };
      }
      return undefined;
    },
    observeResponse(op, path, _status, headers) {
      if (!isMd(path)) return;
      if (op === "delete") {
        getSnapshot().syncedHashes.delete(path);
        return;
      }
      if (op === "read" || op === "write") {
        const h = headers.get("X-Content-Hash");
        if (h) getSnapshot().syncedHashes.set(path, h);
      }
    },
  };
  http.addHeaderHook(hook);
}

/**
 * Convenience: keeps the public surface of the sync engine simple — pass any
 * SpacePrimitives instance through if you're not yet on HttpSpacePrimitives.
 */
export function maybeInstall(
  remote: SpacePrimitives,
  getSnapshot: () => SyncSnapshot,
): void {
  // Duck-type rather than instanceof to avoid hard import-time coupling.
  if (
    typeof (remote as any).addHeaderHook === "function"
  ) {
    installHashTrackingHook(remote as HttpSpacePrimitives, getSnapshot);
  }
}
