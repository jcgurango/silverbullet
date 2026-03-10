import type { DocManager } from "./doc_manager.ts";
import { relative } from "@std/path";

const DEBOUNCE_MS = 500;

export class FileWatcher {
  private debounceTimers = new Map<string, number>();
  private watcher: Deno.FsWatcher | undefined;

  constructor(
    private spaceFolder: string,
    private docManager: DocManager,
  ) {}

  async start(): Promise<void> {
    this.watcher = Deno.watchFs(this.spaceFolder, { recursive: true });
    console.log(`File watcher started on ${this.spaceFolder}`);

    for await (const event of this.watcher) {
      for (const path of event.paths) {
        // Only watch .md files
        if (!path.endsWith(".md")) continue;

        // Skip .crdt/ directory
        const rel = relative(this.spaceFolder, path);
        if (rel.startsWith(".crdt")) continue;

        // Skip files written by our own flush
        if (this.docManager.isOurWrite(rel)) continue;

        this.debounce(rel, event.kind);
      }
    }
  }

  private debounce(
    docPath: string,
    kind: Deno.FsEvent["kind"],
  ): void {
    const existing = this.debounceTimers.get(docPath);
    if (existing) clearTimeout(existing);

    this.debounceTimers.set(
      docPath,
      setTimeout(() => {
        this.debounceTimers.delete(docPath);
        this.handleEvent(docPath, kind);
      }, DEBOUNCE_MS),
    );
  }

  private handleEvent(docPath: string, kind: Deno.FsEvent["kind"]): void {
    switch (kind) {
      case "modify":
      case "create":
        console.log(`External change detected: ${docPath}`);
        this.docManager.handleExternalChange(docPath).catch((e) =>
          console.error(`Error handling external change for ${docPath}:`, e)
        );
        break;
      case "remove":
        console.log(`External delete detected: ${docPath}`);
        this.docManager.handleExternalDelete(docPath).catch((e) =>
          console.error(`Error handling external delete for ${docPath}:`, e)
        );
        break;
    }
  }

  stop(): void {
    this.watcher?.close();
    for (const timer of this.debounceTimers.values()) {
      clearTimeout(timer);
    }
    this.debounceTimers.clear();
  }
}
