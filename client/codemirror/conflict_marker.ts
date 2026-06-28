/**
 * CodeMirror 6 extension for inline conflict marker resolution.
 * Renders VSCode-style action buttons above each conflict section
 * and highlights the server/client regions with distinct colors.
 */
import { type EditorState, StateField } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  WidgetType,
} from "@codemirror/view";
import type { Range } from "@codemirror/state";

/** Parsed location of a single conflict section within the document. */
interface ConflictSection {
  /** Start of the <<<<<<< line */
  from: number;
  /** End of the >>>>>>> line (including newline if present) */
  to: number;
  /** Start of server content (line after <<<<<<<) */
  serverFrom: number;
  /** End of server content (line before =======) */
  serverTo: number;
  /** Start of client content (line after =======) */
  clientFrom: number;
  /** End of client content (line before >>>>>>>) */
  clientTo: number;
}

/** Find all conflict sections in the document. */
function findConflictSections(state: EditorState): ConflictSection[] {
  const doc = state.doc;
  const sections: ConflictSection[] = [];
  let i = 1;
  while (i <= doc.lines) {
    const line = doc.line(i);
    if (line.text.startsWith("<<<<<<< ")) {
      const from = line.from;
      const serverFrom = line.to + 1;
      // Find =======
      let j = i + 1;
      let separatorLine = null;
      while (j <= doc.lines) {
        const l = doc.line(j);
        if (l.text === "=======") {
          separatorLine = l;
          break;
        }
        if (l.text.startsWith(">>>>>>> ")) break; // malformed
        j++;
      }
      if (!separatorLine) { i++; continue; }
      const serverTo = separatorLine.from;
      const clientFrom = separatorLine.to + 1;
      // Find >>>>>>>
      let k = j + 1;
      let endLine = null;
      while (k <= doc.lines) {
        const l = doc.line(k);
        if (l.text.startsWith(">>>>>>> ")) {
          endLine = l;
          break;
        }
        if (l.text.startsWith("<<<<<<< ")) break; // nested/malformed
        k++;
      }
      if (!endLine) { i++; continue; }
      const clientTo = endLine.from;
      // Include the trailing newline of >>>>>>> if it exists
      const to = endLine.to < doc.length ? endLine.to + 1 : endLine.to;
      sections.push({
        from,
        to,
        serverFrom,
        serverTo,
        clientFrom,
        clientTo,
      });
      i = k + 1;
    } else {
      i++;
    }
  }
  return sections;
}

/** Widget that renders action buttons above a conflict section. */
class ConflictActionWidget extends WidgetType {
  constructor(
    private section: ConflictSection,
  ) {
    super();
  }

  override eq(other: WidgetType): boolean {
    return other instanceof ConflictActionWidget &&
      this.section.from === (other as ConflictActionWidget).section.from;
  }

  override toDOM(view: EditorView): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "sb-conflict-actions";

    const label = document.createElement("span");
    label.className = "sb-conflict-label";
    label.textContent = "Merge Conflict";
    wrap.appendChild(label);

    const btnServer = document.createElement("button");
    btnServer.className = "sb-conflict-btn";
    btnServer.textContent = "Accept Server";
    btnServer.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      this.resolve(view, "server");
    });
    wrap.appendChild(btnServer);

    const btnClient = document.createElement("button");
    btnClient.className = "sb-conflict-btn";
    btnClient.textContent = "Accept Yours";
    btnClient.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      this.resolve(view, "client");
    });
    wrap.appendChild(btnClient);

    const btnBoth = document.createElement("button");
    btnBoth.className = "sb-conflict-btn";
    btnBoth.textContent = "Accept Both";
    btnBoth.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      this.resolve(view, "both");
    });
    wrap.appendChild(btnBoth);

    return wrap;
  }

  override ignoreEvent(): boolean {
    return true;
  }

  private resolve(
    view: EditorView,
    choice: "server" | "client" | "both",
  ) {
    const doc = view.state.doc;
    // Re-find the section in the current document state (it may have shifted)
    const sections = findConflictSections(view.state);
    const section = sections.find((s) => s.from === this.section.from);
    if (!section) return; // Already resolved

    let replacement: string;
    const serverContent = doc.sliceString(section.serverFrom, section.serverTo);
    const clientContent = doc.sliceString(section.clientFrom, section.clientTo);

    switch (choice) {
      case "server":
        replacement = serverContent;
        break;
      case "client":
        replacement = clientContent;
        break;
      case "both":
        replacement = serverContent +
          (serverContent && clientContent ? "\n" : "") +
          clientContent;
        break;
    }

    view.dispatch({
      changes: { from: section.from, to: section.to, insert: replacement },
    });
  }
}

/** Build decorations for all conflict sections. */
function buildConflictDecorations(state: EditorState): DecorationSet {
  const sections = findConflictSections(state);
  if (sections.length === 0) return Decoration.none;

  const decorations: Range<Decoration>[] = [];

  for (const section of sections) {
    // Action buttons widget above the conflict
    decorations.push(
      Decoration.widget({
        widget: new ConflictActionWidget(section),
        block: true,
        side: -1,
      }).range(section.from),
    );

    // Line decorations for marker lines and content regions
    const doc = state.doc;

    // <<<<<<< line
    const startLine = doc.lineAt(section.from);
    decorations.push(
      Decoration.line({ class: "sb-conflict-marker-line" }).range(
        startLine.from,
      ),
    );

    // Server content lines
    if (section.serverFrom < section.serverTo) {
      let pos = section.serverFrom;
      while (pos < section.serverTo) {
        const line = doc.lineAt(pos);
        decorations.push(
          Decoration.line({ class: "sb-conflict-server-line" }).range(
            line.from,
          ),
        );
        pos = line.to + 1;
      }
    }

    // ======= line
    if (section.serverTo <= doc.length) {
      const sepLine = doc.lineAt(section.serverTo);
      decorations.push(
        Decoration.line({ class: "sb-conflict-marker-line" }).range(
          sepLine.from,
        ),
      );
    }

    // Client content lines
    if (section.clientFrom < section.clientTo) {
      let pos = section.clientFrom;
      while (pos < section.clientTo) {
        const line = doc.lineAt(pos);
        decorations.push(
          Decoration.line({ class: "sb-conflict-client-line" }).range(
            line.from,
          ),
        );
        pos = line.to + 1;
      }
    }

    // >>>>>>> line
    const endLineStart = section.to < doc.length
      ? section.to - 1
      : section.to;
    if (endLineStart >= 0) {
      const endLine = doc.lineAt(
        Math.min(endLineStart, doc.length),
      );
      if (endLine.text.startsWith(">>>>>>> ")) {
        decorations.push(
          Decoration.line({ class: "sb-conflict-marker-line" }).range(
            endLine.from,
          ),
        );
      }
    }
  }

  return Decoration.set(decorations, true);
}

/** StateField that tracks conflict sections and provides decorations. */
export const conflictMarkerField = StateField.define<DecorationSet>({
  create(state) {
    return buildConflictDecorations(state);
  },

  update(value, tr) {
    if (!tr.docChanged) return value;
    return buildConflictDecorations(tr.state);
  },

  provide: (f) => EditorView.decorations.from(f),
});

/** Theme for conflict marker styling. */
export const conflictMarkerTheme = EditorView.baseTheme({
  ".sb-conflict-actions": {
    display: "flex",
    alignItems: "center",
    gap: "6px",
    padding: "4px 8px",
    borderBottom: "1px solid var(--editor-code-background-color, #e0e0e0)",
    fontSize: "12px",
    flexWrap: "wrap",
  },
  ".sb-conflict-label": {
    fontWeight: "bold",
    color: "var(--meta-color, #650007)",
    marginRight: "4px",
  },
  ".sb-conflict-btn": {
    padding: "2px 8px",
    border: "1px solid var(--button-border-color, #999)",
    borderRadius: "3px",
    background: "var(--button-background-color, #eee)",
    color: "var(--button-color, black)",
    cursor: "pointer",
    fontSize: "12px",
    lineHeight: "1.4",
    "&:hover": {
      background: "var(--button-hover-background-color, #ddd)",
    },
  },
  ".sb-conflict-marker-line": {
    backgroundColor: "var(--subtle-background-color, rgba(72, 72, 72, 0.1))",
    color: "var(--meta-subtle-color, #959595)",
  },
  ".sb-conflict-server-line": {
    backgroundColor: "rgba(255, 200, 100, 0.15)",
  },
  ".sb-conflict-client-line": {
    backgroundColor: "rgba(100, 200, 255, 0.15)",
  },

  // Dark mode overrides
  "&dark .sb-conflict-server-line": {
    backgroundColor: "rgba(255, 200, 100, 0.1)",
  },
  "&dark .sb-conflict-client-line": {
    backgroundColor: "rgba(100, 200, 255, 0.1)",
  },
});

/** Extension bundle to add conflict marker support to an editor. */
export function conflictMarkerExtension() {
  return [conflictMarkerField, conflictMarkerTheme];
}
