import { compile as gitIgnoreCompiler } from "gitignore-parser";
import { jitter, sleep } from "@silverbulletmd/silverbullet/lib/async";
import type { KvPrimitives } from "../data/kv_primitives.ts";
import { EventEmitter } from "../plugos/event.ts";
import { stdLibPrefix } from "../spaces/constants.ts";
import type { SpacePrimitives } from "../spaces/space_primitives.ts";
import { SpaceSync, SyncSnapshot, type SyncStatus } from "../spaces/sync.ts";
import type { HttpSpacePrimitives } from "../spaces/http_space_primitives.ts";
import { MergeConflictError } from "../spaces/merge_conflict.ts";
import { maybeInstall as installHashTrackingHook } from "./hash_tracking_space_primitives.ts";

const syncSnapshotKey = ["$sync", "snapshot"];
const syncInterval = 20;

type SyncEngineEvents = {
  // Full sync cycle has completed
  spaceSyncComplete: (operations: number) => void | Promise<void>;

  // A single file syncle has completed
  fileSyncComplete: (path: string, operations: number) => void | Promise<void>;

  syncError: (error: Error, path?: string) => void | Promise<void>;

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

  async start() {
    this.snapshot = await this.loadSnapshot();

    // Wire the version-history hash protocol via a generic header hook on the
    // HTTP space. No-op when `remote` isn't an HttpSpacePrimitives.
    installHashTrackingHook(this.remote, () => this.snapshot);

    this.spaceSync = new SpaceSync(this.local, this.remote, {
      conflictResolver: this.versionedAwareConflictResolver.bind(this),
      isSyncCandidate: this.isSyncCandidate.bind(this),
    });

    this.spaceSync.on({
      syncProgress: async (status, snapshot) => {
        void this.emit("syncProgress", status, snapshot);
        await this.saveSnapshot(snapshot);
      },
      snapshotUpdated: this.saveSnapshot.bind(this),
    });

    // Start the sync loop
    void this.run();
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
    console.log("[sync] Updated sync config:", this.syncConfig);
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
        void this.emit("spaceSyncComplete", operations);
      }
      return operations;
    } catch (e) {
      void this.emit("syncError", e);
      throw e;
    }
  }

  async syncSingleFile(path: string): Promise<number> {
    try {
      const operations = await this.spaceSync.syncSingleFile(
        path,
        this.snapshot,
      );
      void this.emit("fileSyncComplete", path, operations);
      return operations;
    } catch (e) {
      void this.emit("syncError", e, path);
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
    return this.kv.batchSet([
      {
        key: syncSnapshotKey,
        value: snapshot.toJSON(),
      },
    ]);
  }

  async wipe() {
    this.stop();
    console.log("Wiping sync database");
    await this.kv.clear();
    console.log("Done wiping");
  }

  /**
   * Wrapper that intercepts .md conflicts and delegates to the server's
   * 3-way merge (via the X-Parent-Hash protocol). Non-.md files (including the
   * stdlib plug case) fall through to the existing resolver. Keeping the
   * original method untouched insulates this feature from upstream edits.
   */
  async versionedAwareConflictResolver(
    name: string,
    snapshot: SyncSnapshot,
    primary: SpacePrimitives,
    secondary: SpacePrimitives,
  ): Promise<number> {
    if (name.endsWith(".md") && !name.startsWith(stdLibPrefix)) {
      return this.serverSideMergeResolver(name, snapshot, primary, secondary);
    }
    return this.stdLibAwareConflictResolver(name, snapshot, primary, secondary);
  }

  /**
   * For .md files: push local to server (HttpSpacePrimitives auto-attaches the
   * parent hash). On HTTP 409 the writeFile throws a MergeConflictError carrying
   * the conflict-marked content; persist it locally + freeze the file until the
   * user resolves the markers.
   */
  private async serverSideMergeResolver(
    name: string,
    snapshot: SyncSnapshot,
    primary: SpacePrimitives,
    secondary: SpacePrimitives,
  ): Promise<number> {
    console.log("[sync]", "Using server-side merge for", name);
    const { data: localData, meta: localMeta } = await primary.readFile(name);
    try {
      await secondary.writeFile(name, localData, localMeta);
      // The server merged cleanly; pull the merged content back to local.
      const { data: mergedData, meta: remoteMeta } = await secondary.readFile(
        name,
      );
      const newLocalMeta = await primary.writeFile(name, mergedData, remoteMeta);
      snapshot.files.set(name, [
        newLocalMeta.lastModified,
        remoteMeta.lastModified,
      ]);
      console.log("[sync]", "Server-side merge succeeded for", name);
      return 1;
    } catch (e: any) {
      if (e instanceof MergeConflictError) {
        console.warn("[sync]", "Server-side merge conflict for", name);
        const newLocalMeta = await primary.writeFile(
          name,
          e.conflictContent,
          localMeta,
        );
        snapshot.conflictedFiles.add(name);
        if (e.serverHash) {
          snapshot.syncedHashes.set(name, e.serverHash);
        }
        const existing = snapshot.files.get(name);
        snapshot.files.set(name, [
          newLocalMeta.lastModified,
          existing ? existing[1] : 0,
        ]);
        void this.emit("syncConflict", name);

        // Broadcast to all clients so the inline merge UI can pop up.
        const conflictText = new TextDecoder().decode(e.conflictContent);
        (self as any).clients?.matchAll({ type: "window" })
          .then((clients: any[]) => {
            for (const c of clients) {
              c.postMessage({
                type: "merge-conflict",
                path: name,
                conflictContent: conflictText,
                serverHash: e.serverHash,
              });
            }
          })
          .catch(() => {});
        return 1;
      }
      throw e;
    }
  }

  /**
   * Delegates to the standard primary conflict resolver, but in case of any conflicts in plugs, it will always take the version from the secondary.
   */
  async stdLibAwareConflictResolver(
    name: string,
    snapshot: SyncSnapshot,
    primary: SpacePrimitives,
    secondary: SpacePrimitives,
  ): Promise<number> {
    if (!name.startsWith(stdLibPrefix)) {
      const operations = await SpaceSync.primaryConflictResolver(
        name,
        snapshot,
        primary,
        secondary,
      );

      if (operations > 0) {
        // Something happened -> conflict copy generated, let's report it
        void this.emit("syncConflict", name);
      }

      return operations;
    }
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
    snapshot.files.set(name, [newMeta.lastModified, meta.lastModified]);

    return 1;
  }
}
