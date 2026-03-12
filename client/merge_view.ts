import { MergeView } from "@codemirror/merge";
import { EditorView, keymap } from "@codemirror/view";
import { EditorState } from "@codemirror/state";
import { parseConflictMarkers } from "./spaces/merge_conflict.ts";
import type { Client } from "./client.ts";

/**
 * Shows a side-by-side merge conflict resolution view.
 *
 * @param client - The SilverBullet client instance
 * @param path - The file path that has the conflict
 * @param localText - The user's current editor content (theirs)
 * @param conflictContent - The conflict-marked content from the server
 * @param serverHash - The server's current HEAD hash
 */
export function showMergeView(
  client: Client,
  path: string,
  localText: string,
  conflictContent: string,
  serverHash: string | null,
) {
  // Parse the conflict-marked content to extract both versions
  const { server: serverText, client: clientText, hasConflicts } =
    parseConflictMarkers(conflictContent);

  // If no actual conflict markers (shouldn't happen but be safe), use the raw content
  const displayServerText = hasConflicts ? serverText : conflictContent;
  const displayClientText = hasConflicts ? clientText : localText;

  // Create the merge view container
  const container = document.createElement("div");
  container.className = "sb-merge-view-container";
  container.innerHTML = `
    <div class="sb-merge-view-header">
      <div class="sb-merge-view-title">
        <strong>Merge Conflict</strong> — ${path.replace(/\.md$/, "")}
      </div>
      <div class="sb-merge-view-labels">
        <span class="sb-merge-view-label sb-merge-view-label-server">Server version</span>
        <span class="sb-merge-view-label sb-merge-view-label-client">Your version</span>
      </div>
      <div class="sb-merge-view-actions">
        <button class="sb-merge-view-btn sb-merge-view-btn-accept-server">Accept Server</button>
        <button class="sb-merge-view-btn sb-merge-view-btn-accept-client">Accept Client</button>
        <button class="sb-merge-view-btn sb-merge-view-btn-save sb-merge-view-btn-primary">Save Resolved</button>
        <button class="sb-merge-view-btn sb-merge-view-btn-cancel">Cancel</button>
      </div>
    </div>
    <div class="sb-merge-view-editor"></div>
  `;

  // Inject styles
  injectMergeViewStyles();

  // Add to DOM
  const editorParent = client.editorView.dom.parentElement!;
  // Hide the regular editor
  client.editorView.dom.style.display = "none";
  editorParent.appendChild(container);

  const editorContainer = container.querySelector(
    ".sb-merge-view-editor",
  ) as HTMLElement;

  // Create the MergeView
  const mergeView = new MergeView({
    a: {
      doc: displayServerText,
      extensions: [
        EditorView.editable.of(false),
        EditorState.readOnly.of(true),
      ],
    },
    b: {
      doc: displayClientText,
      extensions: [
        keymap.of([{
          key: "Mod-s",
          run: () => {
            saveResolved();
            return true;
          },
        }]),
      ],
    },
    parent: editorContainer,
  });

  // Helper to clean up and restore the regular editor
  function cleanup() {
    container.remove();
    client.editorView.dom.style.display = "";
    mergeView.destroy();
  }

  // Save the resolved content from the right (client) panel
  function saveResolved() {
    const resolvedText = mergeView.b.state.doc.toString();

    // Check if there are remaining conflict markers
    if (
      resolvedText.includes("<<<<<<< ") && resolvedText.includes(">>>>>>> ")
    ) {
      client.flashNotification(
        "Please resolve all conflict markers before saving",
        "error",
      );
      return;
    }

    cleanup();

    // Update the editor with the resolved content
    client.editorView.dispatch({
      changes: {
        from: 0,
        to: client.editorView.state.doc.length,
        insert: resolvedText,
      },
    });

    // Update the parent hash to the server's hash so the next save is a fast-forward
    if (serverHash) {
      client.httpSpacePrimitives.setContentHash(path, serverHash);
    }

    // Mark as unsaved so the auto-save picks it up
    client.ui.viewDispatch({ type: "page-changed" });
    // Trigger immediate save
    client.save(true);
  }

  // Wire up buttons
  container.querySelector(".sb-merge-view-btn-accept-server")!
    .addEventListener("click", () => {
      // Replace client panel with server version
      mergeView.b.dispatch({
        changes: {
          from: 0,
          to: mergeView.b.state.doc.length,
          insert: displayServerText,
        },
      });
    });

  container.querySelector(".sb-merge-view-btn-accept-client")!
    .addEventListener("click", () => {
      // Replace client panel with user's original version
      mergeView.b.dispatch({
        changes: {
          from: 0,
          to: mergeView.b.state.doc.length,
          insert: localText,
        },
      });
    });

  container.querySelector(".sb-merge-view-btn-save")!
    .addEventListener("click", saveResolved);

  container.querySelector(".sb-merge-view-btn-cancel")!
    .addEventListener("click", () => {
      cleanup();
      client.flashNotification(
        "Merge cancelled — your local changes are preserved but not synced",
        "info",
      );
    });
}

let stylesInjected = false;

function injectMergeViewStyles() {
  if (stylesInjected) return;
  stylesInjected = true;

  const style = document.createElement("style");
  style.textContent = `
    .sb-merge-view-container {
      position: absolute;
      top: 0;
      left: 0;
      right: 0;
      bottom: 0;
      z-index: 100;
      display: flex;
      flex-direction: column;
      background: var(--root-background-color, #fff);
    }

    .sb-merge-view-header {
      display: flex;
      align-items: center;
      gap: 12px;
      padding: 8px 16px;
      border-bottom: 1px solid var(--panel-border-color, #ddd);
      background: var(--panel-background-color, #f5f5f5);
      flex-shrink: 0;
      flex-wrap: wrap;
    }

    .sb-merge-view-title {
      font-size: 14px;
    }

    .sb-merge-view-labels {
      display: flex;
      gap: 8px;
      flex: 1;
      justify-content: center;
    }

    .sb-merge-view-label {
      font-size: 12px;
      padding: 2px 8px;
      border-radius: 4px;
    }

    .sb-merge-view-label-server {
      background: #fde68a;
      color: #92400e;
    }

    .sb-merge-view-label-client {
      background: #bbf7d0;
      color: #166534;
    }

    .sb-merge-view-actions {
      display: flex;
      gap: 6px;
    }

    .sb-merge-view-btn {
      padding: 4px 12px;
      border: 1px solid var(--panel-border-color, #ccc);
      border-radius: 4px;
      background: var(--button-background-color, #fff);
      color: var(--root-color, #333);
      cursor: pointer;
      font-size: 13px;
    }

    .sb-merge-view-btn:hover {
      background: var(--button-hover-background-color, #eee);
    }

    .sb-merge-view-btn-primary {
      background: #2563eb;
      color: white;
      border-color: #1d4ed8;
    }

    .sb-merge-view-btn-primary:hover {
      background: #1d4ed8;
    }

    .sb-merge-view-editor {
      flex: 1;
      overflow: auto;
    }

    .sb-merge-view-editor .cm-mergeView {
      height: 100%;
    }

    .sb-merge-view-editor .cm-mergeViewEditors {
      height: 100%;
    }

    .sb-merge-view-editor .cm-mergeViewEditor {
      height: 100%;
      overflow: auto;
    }
  `;
  document.head.appendChild(style);
}
