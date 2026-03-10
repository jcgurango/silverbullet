import * as Y from "yjs";
import diff from "fast-diff";
import { join, dirname, relative } from "@std/path";
import { ensureDir } from "@std/fs";

const CRDT_DIR = ".crdt";
const PERSIST_DEBOUNCE_MS = 1000;
const FLUSH_DEBOUNCE_MS = 1000;
const EVICTION_GRACE_MS = 60_000;

export interface DocEntry {
  doc: Y.Doc;
  conns: Set<WebSocket>;
  persistTimer: number | undefined;
  flushTimer: number | undefined;
  lastAccess: number;
}

export class DocManager {
  private docs = new Map<string, DocEntry>();
  /** Paths recently written by us — file watcher should ignore these */
  private writeGuard = new Map<string, number>();
  private evictionInterval: number;

  constructor(private spaceFolder: string) {
    this.evictionInterval = setInterval(
      () => this.evictIdle(EVICTION_GRACE_MS),
      30_000,
    );
  }

  /** Mark a path as recently written by us (for file watcher to skip) */
  isOurWrite(path: string): boolean {
    const ts = this.writeGuard.get(path);
    if (ts && Date.now() - ts < 2000) {
      this.writeGuard.delete(path);
      return true;
    }
    return false;
  }

  /** Get a loaded doc entry, or undefined */
  getEntry(docPath: string): DocEntry | undefined {
    const entry = this.docs.get(docPath);
    if (entry) {
      entry.lastAccess = Date.now();
    }
    return entry;
  }

  /** Load or create a Yjs doc for the given document path (relative, e.g. "index.md") */
  async loadOrCreate(docPath: string): Promise<DocEntry> {
    const existing = this.docs.get(docPath);
    if (existing) {
      existing.lastAccess = Date.now();
      return existing;
    }

    const doc = new Y.Doc();
    const entry: DocEntry = {
      doc,
      conns: new Set(),
      persistTimer: undefined,
      flushTimer: undefined,
      lastAccess: Date.now(),
    };

    // Try loading from .crdt/ first
    const crdtPath = this.crdtPath(docPath);
    try {
      const crdtData = await Deno.readFile(crdtPath);
      Y.applyUpdate(doc, crdtData);
    } catch {
      // No CRDT state — initialize from .md file
      const mdPath = this.mdPath(docPath);
      try {
        const mdContent = await Deno.readTextFile(mdPath);
        const ytext = doc.getText("content");
        ytext.insert(0, mdContent);
      } catch {
        // Neither exists — start with empty doc
      }
      // Persist initial CRDT state
      await this.persistCrdt(docPath, entry);
    }

    // Set up update listener for broadcasting and persistence
    doc.on("update", (update: Uint8Array, origin: unknown) => {
      // Broadcast to all connected clients except the origin
      for (const conn of entry.conns) {
        if (conn !== origin && conn.readyState === WebSocket.OPEN) {
          const encoder = new Uint8Array(update.length + 1);
          // Message type 0 = sync, but we use y-protocols encoding
          // Actually, we'll handle broadcasting in ws_handler
        }
      }

      // Schedule debounced persistence
      this.schedulePersist(docPath, entry);
      this.scheduleFlush(docPath, entry);
    });

    this.docs.set(docPath, entry);
    return entry;
  }

  /** Apply an external file change (from file watcher) to the Yjs doc */
  async handleExternalChange(docPath: string): Promise<void> {
    const entry = this.docs.get(docPath);
    if (!entry) return; // Doc not loaded, nothing to do

    const mdPath = this.mdPath(docPath);
    let newContent: string;
    try {
      newContent = await Deno.readTextFile(mdPath);
    } catch {
      return; // File might have been deleted
    }

    const ytext = entry.doc.getText("content");
    const currentContent = ytext.toString();

    if (currentContent === newContent) return; // No actual change

    // Compute diff and apply as Yjs operations
    const diffs = diff(currentContent, newContent);
    entry.doc.transact(() => {
      let offset = 0;
      for (const [op, text] of diffs) {
        if (op === diff.INSERT) {
          ytext.insert(offset, text);
          offset += text.length;
        } else if (op === diff.EQUAL) {
          offset += text.length;
        } else if (op === diff.DELETE) {
          ytext.delete(offset, text.length);
        }
      }
    }, "external"); // origin = "external" to distinguish from client edits
  }

  /** Handle external file deletion */
  async handleExternalDelete(docPath: string): Promise<void> {
    const entry = this.docs.get(docPath);
    if (entry) {
      // Close all connections
      for (const conn of entry.conns) {
        conn.close(4404, "File deleted");
      }
      clearTimeout(entry.persistTimer);
      clearTimeout(entry.flushTimer);
      this.docs.delete(docPath);
    }

    // Remove CRDT state
    try {
      await Deno.remove(this.crdtPath(docPath));
    } catch {
      // Already gone
    }
  }

  /** Persist CRDT state to .crdt/ directory */
  async persistCrdt(docPath: string, entry?: DocEntry): Promise<void> {
    entry = entry ?? this.docs.get(docPath);
    if (!entry) return;

    const crdtPath = this.crdtPath(docPath);
    await ensureDir(dirname(crdtPath));
    const state = Y.encodeStateAsUpdate(entry.doc);
    await Deno.writeFile(crdtPath, state);
  }

  /** Flush Yjs text content to .md file */
  async flushToMarkdown(docPath: string, entry?: DocEntry): Promise<void> {
    entry = entry ?? this.docs.get(docPath);
    if (!entry) return;

    const mdPath = this.mdPath(docPath);
    const content = entry.doc.getText("content").toString();

    await ensureDir(dirname(mdPath));
    this.writeGuard.set(docPath, Date.now());
    await Deno.writeTextFile(mdPath, content);
  }

  /** Run startup reconciliation — sync .crdt/ state with .md files */
  async reconcile(): Promise<void> {
    try {
      await this.reconcileDir("");
    } catch {
      // .crdt/ directory might not exist yet
    }
  }

  private async reconcileDir(relDir: string): Promise<void> {
    const crdtDir = join(this.spaceFolder, CRDT_DIR, relDir);
    for await (const entry of Deno.readDir(crdtDir)) {
      const relPath = relDir ? `${relDir}/${entry.name}` : entry.name;
      if (entry.isDirectory) {
        await this.reconcileDir(relPath);
        continue;
      }
      if (!entry.name.endsWith(".yjs")) continue;

      // Convert .yjs path back to .md path
      const docPath = relPath.replace(/\.yjs$/, ".md");
      const mdPath = this.mdPath(docPath);

      try {
        await Deno.stat(mdPath);
        // .md exists — load and reconcile
        const docEntry = await this.loadOrCreate(docPath);
        await this.handleExternalChange(docPath);
        // Evict immediately since we're just reconciling
        if (docEntry.conns.size === 0) {
          await this.persistCrdt(docPath, docEntry);
          await this.flushToMarkdown(docPath, docEntry);
          this.docs.delete(docPath);
        }
      } catch {
        // .md doesn't exist — remove orphaned CRDT state
        const crdtPath = this.crdtPath(docPath);
        console.log(`Removing orphaned CRDT state: ${crdtPath}`);
        await Deno.remove(crdtPath).catch(() => {});
      }
    }
  }

  /** Evict docs with no connections that have been idle */
  private evictIdle(maxIdleMs: number): void {
    const now = Date.now();
    for (const [docPath, entry] of this.docs) {
      if (entry.conns.size === 0 && now - entry.lastAccess > maxIdleMs) {
        // Flush before evicting
        this.persistCrdt(docPath, entry);
        this.flushToMarkdown(docPath, entry);
        clearTimeout(entry.persistTimer);
        clearTimeout(entry.flushTimer);
        this.docs.delete(docPath);
        console.log(`Evicted idle doc: ${docPath}`);
      }
    }
  }

  private schedulePersist(docPath: string, entry: DocEntry): void {
    clearTimeout(entry.persistTimer);
    entry.persistTimer = setTimeout(() => {
      this.persistCrdt(docPath, entry);
    }, PERSIST_DEBOUNCE_MS);
  }

  private scheduleFlush(docPath: string, entry: DocEntry): void {
    clearTimeout(entry.flushTimer);
    entry.flushTimer = setTimeout(() => {
      this.flushToMarkdown(docPath, entry);
    }, FLUSH_DEBOUNCE_MS);
  }

  private crdtPath(docPath: string): string {
    return join(this.spaceFolder, CRDT_DIR, docPath.replace(/\.md$/, ".yjs"));
  }

  private mdPath(docPath: string): string {
    return join(this.spaceFolder, docPath);
  }

  /** Flush all docs and clean up */
  async shutdown(): Promise<void> {
    clearInterval(this.evictionInterval);
    for (const [docPath, entry] of this.docs) {
      clearTimeout(entry.persistTimer);
      clearTimeout(entry.flushTimer);
      await this.persistCrdt(docPath, entry);
      await this.flushToMarkdown(docPath, entry);
      for (const conn of entry.conns) {
        conn.close(1001, "Server shutting down");
      }
    }
    this.docs.clear();
  }
}
