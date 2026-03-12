import { compile as gitIgnoreCompiler } from "gitignore-parser";
import { jitter, sleep } from "@silverbulletmd/silverbullet/lib/async";
import type { KvPrimitives } from "../data/kv_primitives.ts";
import { EventEmitter } from "../plugos/event.ts";
import { stdLibPrefix } from "../spaces/constants.ts";
import type { SpacePrimitives } from "../spaces/space_primitives.ts";
import { SpaceSync, SyncSnapshot, type SyncStatus } from "../spaces/sync.ts";
import type { HttpSpacePrimitives } from "../spaces/http_space_primitives.ts";
import { MergeConflictError } from "../spaces/merge_conflict.ts";
import type { FileMeta } from "@silverbulletmd/silverbullet/type/index";

const syncSnapshotKey = ["$sync", "snapshot"];
const syncInterval = 20;

type SyncEngineEvents = {
  // Full sync cycle has completed
  spaceSyncComplete: (operations: number) => void | Promise<void>;

  // A single file syncle has completed
  fileSyncComplete: (path: string, operations: number) => void | Promise<void>;

  syncError: (error: Error) => void | Promise<void>;

  // Sync conflict occurred
  syncConflict: (path: string) => void | Promise<void>;

  // Sync progress updated
  syncProgress: (
    syncStatus: SyncStatus,
    snapshot: SyncSnapshot,
  ) => void | Promise<void>;
};

export type SyncConfig = {
  syncDocuments?: boolean;
  syncIgnore?: string;
};

/**
 * Wraps HttpSpacePrimitives to automatically manage hash tracking via the sync snapshot.
 * Before writes: sets parent hash from snapshot.syncedHashes
 * After reads/writes: captures content hash into snapshot.syncedHashes
 */
class HashTrackingSpacePrimitives implements SpacePrimitives {
  constructor(
    private inner: HttpSpacePrimitives,
    private getSnapshot: () => SyncSnapshot,
  ) {}

  fetchFileList(): Promise<FileMeta[]> {
    return this.inner.fetchFileList();
  }

  getFileMeta(path: string, observing?: boolean): Promise<FileMeta> {
    // Intentionally no hash tracking on getFileMeta — it's like "git fetch",
    // just reconnaissance. Only actual content operations update hash state.
    return this.inner.getFileMeta(path, observing);
  }

  async readFile(path: string): Promise<{ data: Uint8Array; meta: FileMeta }> {
    const result = await this.inner.readFile(path);

    // After pull: update synced hash (like "git pull" updates HEAD)
    if (path.endsWith(".md")) {
      const hash = this.inner.consumeContentHash(path);
      if (hash) {
        this.getSnapshot().syncedHashes.set(path, hash);
      }
    }

    return result;
  }

  async writeFile(
    path: string,
    data: Uint8Array,
    meta?: FileMeta,
  ): Promise<FileMeta> {
    // Before push: set parent hash from snapshot (like "git push" uses local HEAD)
    if (path.endsWith(".md")) {
      const parentHash = this.getSnapshot().syncedHashes.get(path);
      this.inner.setParentHash(path, parentHash);
    }

    const result = await this.inner.writeFile(path, data, meta);

    // After push: update synced hash (like "git push" succeeding updates remote tracking)
    if (path.endsWith(".md")) {
      const hash = this.inner.consumeContentHash(path);
      if (hash) {
        this.getSnapshot().syncedHashes.set(path, hash);
      }
    }

    return result;
  }

  async deleteFile(path: string): Promise<void> {
    await this.inner.deleteFile(path);
    // Clean up hash tracking on delete
    this.getSnapshot().syncedHashes.delete(path);
    this.getSnapshot().conflictedFiles.delete(path);
  }
}

/**
 * Thin wrapper around SpaceSync, adds snapshot persistence and a few other things
 */
export class SyncEngine extends EventEmitter<SyncEngineEvents> {
  spaceSync!: SpaceSync;

  private syncConfig: SyncConfig = {
    syncDocuments: true,
  };

  stopping = false;
  syncAccepts: (path: string) => boolean = () => true;
  snapshot!: SyncSnapshot;

  constructor(
    private kv: KvPrimitives,
    readonly local: SpacePrimitives,
    readonly remote: HttpSpacePrimitives,
  ) {
    super();
  }

  /**
   * Get the synced content hash for a file from the persisted snapshot.
   * Used by proxy_router to include X-Content-Hash in responses.
   */
  getSyncedHash(path: string): string | undefined {
    return this.snapshot?.syncedHashes.get(path);
  }

  async start() {
    this.snapshot = await this.loadSnapshot();

    // Wrap remote in hash-tracking layer that auto-manages syncedHashes
    const hashTrackedRemote = new HashTrackingSpacePrimitives(
      this.remote,
      () => this.snapshot,
    );

    this.spaceSync = new SpaceSync(this.local, hashTrackedRemote, {
      conflictResolver: this.stdLibAwareConflictResolver.bind(this),
      isSyncCandidate: this.isSyncCandidate.bind(this),
    });

    this.spaceSync.on({
      syncProgress: async (status, snapshot) => {
        this.emit("syncProgress", status, snapshot);
        await this.saveSnapshot(snapshot);
      },
      snapshotUpdated: this.saveSnapshot.bind(this),
    });

    // Start the sync loop
    this.run();
  }

  stop() {
    this.stopping = true;
  }

  async run() {
    while (true) {
      if (this.stopping) {
        return;
      }
      try {
        await this.syncSpace();
      } catch (e: any) {
        // User error communication is happening in syncSpace
        console.error("Sync space error", e.message);
      }
      await sleep(syncInterval * 1000 + jitter());
    }
  }

  public setSyncConfig(config: SyncConfig) {
    this.syncConfig = config;
    this.syncAccepts = config.syncIgnore
      ? gitIgnoreCompiler(config.syncIgnore).accepts
      : () => true;
    console.log(
      "[sync] Updated sync config:",
      this.syncConfig,
    );
  }

  isSyncCandidate(path: string): boolean {
    // ALWAYS sync plugs
    if (path.endsWith(".plug.js")) {
      return true;
    }
    // Follow SB_SYNC_IGNORE rules
    if (!this.syncAccepts(path)) {
      return false;
    }
    // Either sync all files, or only .md files if syncDocuments is false
    return this.syncConfig.syncDocuments || path.endsWith(".md");
  }

  async syncSpace(): Promise<number> {
    try {
      const operations = await this.spaceSync.syncFiles(this.snapshot);
      if (operations !== -1) {
        // emit successful sync event (not when operations === -1, because that means another sync was ongoing)
        this.emit("spaceSyncComplete", operations);
      }
      return operations;
    } catch (e) {
      this.emit("syncError", e);
      throw e;
    }
  }

  async syncSingleFile(path: string): Promise<number> {
    try {
      const operations = await this.spaceSync.syncSingleFile(
        path,
        this.snapshot,
      );
      this.emit("fileSyncComplete", path, operations);
      return operations;
    } catch (e) {
      this.emit("syncError", e);
      throw e;
    }
  }

  /**
   * Loads the sync snapshot from the data store.
   * @returns A map of sync status items.
   */
  async loadSnapshot(): Promise<SyncSnapshot> {
    const [snapshot] = await this.kv.batchGet([syncSnapshotKey]);
    return SyncSnapshot.fromJSON(snapshot);
  }

  /**
   * Saves the sync snapshot to the data store.
   * @param snapshot A map of sync status items.
   */
  saveSnapshot(snapshot: SyncSnapshot) {
    return this.kv.batchSet([{
      key: syncSnapshotKey,
      value: snapshot.toJSON(),
    }]);
  }

  async wipe() {
    this.stop();
    console.log("Wiping sync database");
    await this.kv.clear();
    console.log("Done wiping");
  }

  /**
   * Delegates to the standard primary conflict resolver, but in case of any conflicts in plugs, it will always take the version from the secondary.
   * For .md files, delegates to server-side 3-way merge instead of creating .conflicted copies.
   */
  async stdLibAwareConflictResolver(
    name: string,
    snapshot: SyncSnapshot,
    primary: SpacePrimitives,
    secondary: SpacePrimitives,
  ): Promise<number> {
    if (name.startsWith(stdLibPrefix)) {
      console.log(
        "[sync]",
        "Conflict in plug",
        name,
        "will pick the version from secondary and be done with it.",
      );
      // Read file from secondary
      const { data, meta } = await secondary.readFile(name);
      // Write file to primary
      const newMeta = await primary.writeFile(name, data, meta);
      // Update snapshot
      snapshot.files.set(name, [
        newMeta.lastModified,
        meta.lastModified,
      ]);
      return 1;
    }

    // For .md files: let the server's versioned write handle the merge
    if (name.endsWith(".md")) {
      return this.serverSideMergeResolver(
        name,
        snapshot,
        primary,
        secondary,
      );
    }

    // For non-.md files: use the old primary-wins conflict resolver
    const operations = await SpaceSync.primaryConflictResolver(
      name,
      snapshot,
      primary,
      secondary,
    );

    if (operations > 0) {
      // Something happened -> conflict copy generated, let's report it
      this.emit("syncConflict", name);
    }

    return operations;
  }

  /**
   * Conflict resolver for .md files that delegates to the server's 3-way merge.
   * Instead of creating .conflicted copies, pushes local version to server
   * which will either merge cleanly or return a 409 (MergeConflictError).
   *
   * On clean merge: pulls merged content back to local, updates snapshot.
   * On conflict: writes conflict-marked content to local, freezes file from sync.
   */
  private async serverSideMergeResolver(
    name: string,
    snapshot: SyncSnapshot,
    primary: SpacePrimitives,
    secondary: SpacePrimitives,
  ): Promise<number> {
    console.log(
      "[sync]",
      "Using server-side merge for",
      name,
    );

    // Read local version
    const { data: localData, meta: localMeta } = await primary.readFile(name);

    try {
      // Push to server — this triggers the server's versioned write (3-way merge)
      // The HashTrackingSpacePrimitives wrapper will set X-Parent-Hash from snapshot
      const writtenMeta = await secondary.writeFile(name, localData, localMeta);

      // Server merged cleanly. Read back the merged content to sync locally.
      const { data: mergedData, meta: remoteMeta } = await secondary.readFile(
        name,
      );
      const newLocalMeta = await primary.writeFile(name, mergedData, remoteMeta);

      // Update snapshot
      snapshot.files.set(name, [
        newLocalMeta.lastModified,
        remoteMeta.lastModified,
      ]);

      console.log("[sync]", "Server-side merge succeeded for", name);
      return 1;
    } catch (e: any) {
      if (e instanceof MergeConflictError) {
        console.warn("[sync]", "Server-side merge conflict for", name);

        // Write conflict-marked content to local so user can resolve it
        const newLocalMeta = await primary.writeFile(
          name,
          e.conflictContent,
          localMeta,
        );

        // Freeze this file from further sync until conflict markers are resolved
        snapshot.conflictedFiles.add(name);

        // Update synced hash to the server's current hash so that when the user
        // resolves and saves, the next push will use the correct parent
        if (e.serverHash) {
          snapshot.syncedHashes.set(name, e.serverHash);
        }

        // Update snapshot timestamps
        // Use the local meta from writing conflict content as primary timestamp
        // Keep the secondary timestamp as-is (we haven't changed the server)
        const existingSnapshot = snapshot.files.get(name);
        snapshot.files.set(name, [
          newLocalMeta.lastModified,
          existingSnapshot ? existingSnapshot[1] : 0,
        ]);

        // Emit conflict event for UI notification
        this.emit("syncConflict", name);

        // Broadcast merge conflict details to all clients for the merge UI
        // deno-lint-ignore no-explicit-any
        (self as any).clients?.matchAll({ type: "window" }).then(
          // deno-lint-ignore no-explicit-any
          (clients: any[]) => {
            const conflictText = new TextDecoder().decode(e.conflictContent);
            // deno-lint-ignore no-explicit-any
            clients.forEach((client: any) => {
              client.postMessage({
                type: "merge-conflict",
                path: name,
                conflictContent: conflictText,
                serverHash: e.serverHash,
              });
            });
          },
        );

        return 1; // We did write (conflict content) to local
      }
      throw e;
    }
  }
}
