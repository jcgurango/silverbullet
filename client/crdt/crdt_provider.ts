import * as Y from "yjs";
import { WebsocketProvider } from "y-websocket";
import { IndexeddbPersistence } from "y-indexeddb";
import type { Awareness } from "y-protocols/awareness";

export class CrdtProvider {
  doc: Y.Doc;
  ytext: Y.Text;
  wsProvider: WebsocketProvider;
  idbPersistence: IndexeddbPersistence;
  undoManager: Y.UndoManager;
  awareness: Awareness;

  private syncResolve: (() => void) | null = null;
  private syncPromise: Promise<void>;

  constructor(
    wsEndpoint: string,
    docPath: string,
    authToken?: string,
  ) {
    this.doc = new Y.Doc();
    this.ytext = this.doc.getText("content");

    // Build WebSocket URL
    // wsEndpoint is relative like "/.crdt/ws/"
    // Remove trailing slash since y-websocket appends "/${roomname}"
    const wsUrl = new URL(wsEndpoint.replace(/\/+$/, ""), window.location.href);
    wsUrl.protocol = wsUrl.protocol === "https:" ? "wss:" : "ws:";

    const params: Record<string, string> = {};
    if (authToken) {
      params.token = authToken;
    }

    this.wsProvider = new WebsocketProvider(
      wsUrl.href,
      docPath,
      this.doc,
      {
        params,
        // Connect immediately
        connect: true,
      },
    );

    this.awareness = this.wsProvider.awareness;

    // Set local awareness state with device info
    this.awareness.setLocalStateField("user", {
      name: "Device",
      color: this.randomColor(),
    });

    // IndexedDB persistence for offline support
    this.idbPersistence = new IndexeddbPersistence(
      `sb-crdt-${docPath}`,
      this.doc,
    );

    // Undo manager scoped to the text type
    this.undoManager = new Y.UndoManager(this.ytext);

    // Create sync promise
    this.syncPromise = new Promise<void>((resolve) => {
      this.syncResolve = resolve;
    });

    this.wsProvider.on("sync", (synced: boolean) => {
      if (synced && this.syncResolve) {
        this.syncResolve();
        this.syncResolve = null;
      }
    });
  }

  /** Wait for initial sync to complete */
  async waitForSync(timeoutMs = 5000): Promise<boolean> {
    if (this.wsProvider.synced) return true;

    const timeout = new Promise<boolean>((resolve) =>
      setTimeout(() => resolve(false), timeoutMs)
    );
    const synced = this.syncPromise.then(() => true);

    return Promise.race([synced, timeout]);
  }

  get connected(): boolean {
    return this.wsProvider.wsconnected;
  }

  destroy(): void {
    this.wsProvider.destroy();
    this.idbPersistence.destroy();
    this.undoManager.destroy();
    this.doc.destroy();
  }

  private randomColor(): string {
    const colors = [
      "#30bced", "#6eeb83", "#ffbc42", "#ecd444",
      "#ee6352", "#9ac2c9", "#8acb88", "#1be7ff",
    ];
    return colors[Math.floor(Math.random() * colors.length)];
  }
}
