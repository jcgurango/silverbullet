import * as Y from "yjs";
import * as syncProtocol from "y-protocols/sync";
import * as awarenessProtocol from "y-protocols/awareness";
import * as encoding from "lib0/encoding";
import * as decoding from "lib0/decoding";
import type { DocManager, DocEntry } from "./doc_manager.ts";

const MSG_SYNC = 0;
const MSG_AWARENESS = 1;

export class WsHandler {
  private awareness = new Map<string, awarenessProtocol.Awareness>();

  constructor(private docManager: DocManager) {}

  getAwareness(docPath: string, doc: Y.Doc): awarenessProtocol.Awareness {
    let aw = this.awareness.get(docPath);
    if (!aw) {
      aw = new awarenessProtocol.Awareness(doc);
      aw.setLocalState(null);

      // When awareness changes, broadcast to all connections
      aw.on("update", (
        { added, updated, removed }: {
          added: number[];
          updated: number[];
          removed: number[];
        },
      ) => {
        const changedClients = [...added, ...updated, ...removed];
        const entry = this.docManager.getEntry(docPath);
        if (!entry) return;

        const encoder = encoding.createEncoder();
        encoding.writeVarUint(encoder, MSG_AWARENESS);
        encoding.writeVarUint8Array(
          encoder,
          awarenessProtocol.encodeAwarenessUpdate(aw!, changedClients),
        );
        const message = encoding.toUint8Array(encoder);

        for (const conn of entry.conns) {
          if (conn.readyState === WebSocket.OPEN) {
            conn.send(message);
          }
        }
      });

      this.awareness.set(docPath, aw);
    }
    return aw;
  }

  async handleConnection(ws: WebSocket, docPath: string): Promise<void> {
    const entry = await this.docManager.loadOrCreate(docPath);
    entry.conns.add(ws);

    const awareness = this.getAwareness(docPath, entry.doc);

    // Send initial sync step 1
    const encoder = encoding.createEncoder();
    encoding.writeVarUint(encoder, MSG_SYNC);
    syncProtocol.writeSyncStep1(encoder, entry.doc);
    ws.send(encoding.toUint8Array(encoder));

    // Send current awareness state
    const awarenessStates = awareness.getStates();
    if (awarenessStates.size > 0) {
      const awarenessEncoder = encoding.createEncoder();
      encoding.writeVarUint(awarenessEncoder, MSG_AWARENESS);
      encoding.writeVarUint8Array(
        awarenessEncoder,
        awarenessProtocol.encodeAwarenessUpdate(
          awareness,
          Array.from(awarenessStates.keys()),
        ),
      );
      ws.send(encoding.toUint8Array(awarenessEncoder));
    }

    // Set up doc update listener for this connection
    const updateHandler = (update: Uint8Array, origin: unknown) => {
      if (origin === ws) return; // Don't echo back to sender
      if (ws.readyState !== WebSocket.OPEN) return;

      const encoder = encoding.createEncoder();
      encoding.writeVarUint(encoder, MSG_SYNC);
      syncProtocol.writeUpdate(encoder, update);
      ws.send(encoding.toUint8Array(encoder));
    };
    entry.doc.on("update", updateHandler);

    ws.onmessage = (event: MessageEvent) => {
      try {
        const data = new Uint8Array(
          event.data instanceof ArrayBuffer
            ? event.data
            : event.data.buffer ?? event.data,
        );
        const decoder = decoding.createDecoder(data);
        const messageType = decoding.readVarUint(decoder);

        switch (messageType) {
          case MSG_SYNC: {
            const encoder = encoding.createEncoder();
            encoding.writeVarUint(encoder, MSG_SYNC);
            syncProtocol.readSyncMessage(
              decoder,
              encoder,
              entry.doc,
              ws, // origin — so updateHandler knows not to echo back
            );
            // If the encoder has content (sync step 2 reply), send it
            if (encoding.length(encoder) > 1) {
              ws.send(encoding.toUint8Array(encoder));
            }
            break;
          }
          case MSG_AWARENESS: {
            awarenessProtocol.applyAwarenessUpdate(
              awareness,
              decoding.readVarUint8Array(decoder),
              ws,
            );
            break;
          }
        }
      } catch (e) {
        console.error(`Error handling WebSocket message for ${docPath}:`, e);
      }
    };

    ws.onclose = () => {
      entry.doc.off("update", updateHandler);
      entry.conns.delete(ws);
      // Remove awareness state for this client
      awarenessProtocol.removeAwarenessStates(
        awareness,
        [entry.doc.clientID],
        null,
      );

      if (entry.conns.size === 0) {
        // Clean up awareness when no more connections
        this.awareness.delete(docPath);
      }

      console.log(
        `WebSocket closed for ${docPath} (${entry.conns.size} remaining)`,
      );
    };

    ws.onerror = (e) => {
      console.error(`WebSocket error for ${docPath}:`, e);
    };

    console.log(
      `WebSocket connected for ${docPath} (${entry.conns.size} total)`,
    );
  }
}
