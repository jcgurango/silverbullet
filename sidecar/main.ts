import { DocManager } from "./doc_manager.ts";
import { FileWatcher } from "./file_watcher.ts";
import { WsHandler } from "./ws_handler.ts";
import { Auth } from "./auth.ts";

const spaceFolder = Deno.env.get("SB_SPACE_FOLDER");
if (!spaceFolder) {
  console.error("SB_SPACE_FOLDER environment variable is required");
  Deno.exit(1);
}

const port = parseInt(Deno.env.get("SB_CRDT_PORT") ?? "3001", 10);
const authSecret = Deno.env.get("SB_AUTH_SECRET");

// Initialize components
const auth = new Auth(authSecret);
await auth.init();

const docManager = new DocManager(spaceFolder);
const wsHandler = new WsHandler(docManager);
const fileWatcher = new FileWatcher(spaceFolder, docManager);

// Run startup reconciliation
console.log("Running CRDT startup reconciliation...");
await docManager.reconcile();
console.log("Reconciliation complete.");

// Acquire file lock to prevent multiple sidecar instances
const lockPath = `${spaceFolder}/.crdt/.lock`;
try {
  await Deno.mkdir(`${spaceFolder}/.crdt`, { recursive: true });
  const lockFile = await Deno.open(lockPath, {
    create: true,
    write: true,
  });
  // Try to acquire exclusive lock
  await lockFile.lock(true);
  // Keep lockFile open for the lifetime of the process
} catch (e) {
  if (e instanceof Deno.errors.WouldBlock || (e as Error).message?.includes("lock")) {
    console.error("Another CRDT sidecar is already running. Exiting.");
    Deno.exit(1);
  }
  // Lock not supported on this platform, continue anyway
  console.warn("File locking not supported, proceeding without lock.");
}

// Start file watcher in the background
const watcherPromise = fileWatcher.start();

// Start WebSocket server
const server = Deno.serve({ port, hostname: "127.0.0.1" }, async (req) => {
  const url = new URL(req.url);

  // Health check
  if (url.pathname === "/health") {
    return new Response("OK", { status: 200 });
  }

  // WebSocket connections: /ws/{docPath}
  if (url.pathname.startsWith("/ws/")) {
    const docPath = decodeURIComponent(url.pathname.slice(4));

    // Validate that it's a .md file
    if (!docPath.endsWith(".md")) {
      return new Response("Only .md files are supported", { status: 400 });
    }

    // Validate auth token
    const token = url.searchParams.get("token");
    if (!await auth.validateToken(token)) {
      return new Response("Unauthorized", { status: 401 });
    }

    // Upgrade to WebSocket
    const { socket, response } = Deno.upgradeWebSocket(req);

    socket.onopen = () => {
      wsHandler.handleConnection(socket, docPath);
    };

    return response;
  }

  return new Response("Not Found", { status: 404 });
});

console.log(`CRDT sidecar listening on 127.0.0.1:${port}`);

// Graceful shutdown
const shutdownSignal = Deno.addSignalListener("SIGTERM", async () => {
  console.log("CRDT sidecar shutting down...");
  fileWatcher.stop();
  await docManager.shutdown();
  await server.shutdown();
  console.log("CRDT sidecar shutdown complete.");
  Deno.exit(0);
});

Deno.addSignalListener("SIGINT", async () => {
  console.log("CRDT sidecar shutting down...");
  fileWatcher.stop();
  await docManager.shutdown();
  await server.shutdown();
  console.log("CRDT sidecar shutdown complete.");
  Deno.exit(0);
});
