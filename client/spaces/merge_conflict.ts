/**
 * Error thrown when a write to the server results in a merge conflict.
 * The server returns HTTP 409 with the conflict-marked content.
 */
export class MergeConflictError extends Error {
  constructor(
    /** The file path that has a conflict */
    public path: string,
    /** Content with git-style conflict markers */
    public conflictContent: Uint8Array,
    /** The server's current HEAD hash */
    public serverHash: string | null,
  ) {
    super(`Merge conflict in ${path}`);
    this.name = "MergeConflictError";
  }
}

/**
 * Check if a string contains git-style conflict markers.
 */
export function hasConflictMarkers(text: string): boolean {
  return text.includes("\n<<<<<<< ") || text.startsWith("<<<<<<< ");
}

/**
 * Parse git-style conflict markers from text into server and client versions.
 * Non-conflicting sections are the same in both outputs.
 */
export function parseConflictMarkers(
  text: string,
): { server: string; client: string; hasConflicts: boolean } {
  const lines = text.split("\n");
  const serverLines: string[] = [];
  const clientLines: string[] = [];
  let inConflict = false;
  let inServer = false;
  let hasConflicts = false;

  for (const line of lines) {
    if (line.startsWith("<<<<<<< ")) {
      hasConflicts = true;
      inConflict = true;
      inServer = true;
      continue;
    }
    if (inConflict && line === "=======") {
      inServer = false;
      continue;
    }
    if (line.startsWith(">>>>>>> ")) {
      inConflict = false;
      continue;
    }

    if (inConflict) {
      if (inServer) {
        serverLines.push(line);
      } else {
        clientLines.push(line);
      }
    } else {
      serverLines.push(line);
      clientLines.push(line);
    }
  }

  return {
    server: serverLines.join("\n"),
    client: clientLines.join("\n"),
    hasConflicts,
  };
}
