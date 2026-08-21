import AppKit
import DatagrepKit
import SwiftUI

// MARK: - materials

/// `NSVisualEffectView` behind the sidebar.
///
/// SwiftUI's `.ultraThinMaterial` is a *blur of what is behind the view inside
/// the window*; the sidebar needs the `.sidebar` material, which samples the
/// desktop behind the whole window. There is no SwiftUI spelling for that, so
/// this is a bridge for a material, not for a control — the "AppKit only for the
/// grid and the editor" rule is about interactive controls, and this draws
/// nothing and handles no events.
struct VisualEffect: NSViewRepresentable {
    var material: NSVisualEffectView.Material = .sidebar
    var blending: NSVisualEffectView.BlendingMode = .behindWindow
    var emphasized = false

    func makeNSView(context: Context) -> NSVisualEffectView {
        let v = NSVisualEffectView()
        v.material = material
        v.blendingMode = blending
        v.state = .followsWindowActiveState
        v.isEmphasized = emphasized
        return v
    }

    func updateNSView(_ v: NSVisualEffectView, context: Context) {
        v.material = material
        v.blendingMode = blending
        v.isEmphasized = emphasized
    }
}

// MARK: - progress

/// Query progress, drawn from real data and nothing else.
///
/// No 60 fps indeterminate spinner: 30 s of "working…" is 1 800 frames of GPU
/// work for zero information. This bar moves **only** when a
/// `DatagrepProgressFn` callback lands. If the block declared `-- @limit N` it
/// is a true determinate bar; otherwise the segment advances one notch per
/// progress event, so motion is literally proportional to rows arriving. When
/// the query is terminal the bar is gone and nothing animates.
struct QueryProgressBar: View {
    @ObservedObject var model: AppModel

    private var fraction: Double? {
        guard let limit = model.directives.limit, limit > 0 else { return nil }
        return min(1, Double(model.rowsLoaded) / Double(limit))
    }

    var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule().fill(Color.secondary.opacity(0.14))
                if let f = fraction {
                    Capsule()
                        .fill(Color.accentColor)
                        .frame(width: max(3, geo.size.width * f))
                } else {
                    // One notch per progress callback, wrapping. No timer.
                    Capsule()
                        .fill(Color.accentColor)
                        .frame(width: geo.size.width * 0.28)
                        .offset(x: geo.size.width * 0.72 * model.progressPhase)
                }
            }
            .animation(.smooth(duration: 0.35), value: model.progressPhase)
            .animation(.smooth(duration: 0.35), value: model.rowsLoaded)
        }
        .frame(height: 3)
    }
}

// MARK: - states

/// What the results pane shows when there is no grid worth showing.
struct ResultsEmptyState: View {
    @ObservedObject var model: AppModel
    /// Observed directly, because whether an editor is open is published by the
    /// tabs model rather than by `AppModel`, and this view has to re-render
    /// when the last tab closes.
    @ObservedObject var tabs: EditorTabsModel

    var body: some View {
        if model.activeProfile.isEmpty {
            ContentUnavailableView {
                Label("No connection", systemImage: "cable.connector.slash")
            } description: {
                Text("Add a database and datagrep will list its schema one level at a time — it never crawls.")
            } actions: {
                Button("New Connection…") { model.showNewConnection = true }
                    .buttonStyle(.borderedProminent)
            }
        } else if model.state == nil {
            // Only while there IS a statement to run. With no editor open the
            // pane below already says "No editor open", and this one then told
            // the user, in the largest type in the window, to press ⌘↩ to run
            // a statement under a caret that does not exist.
            if !tabs.tabs.isEmpty {
                ContentUnavailableView {
                    Label("No result yet", systemImage: "command")
                } description: {
                    Text("Press ⌘↩ to run the statement under the caret.")
                }
            }
        } else if model.rowsLoaded == 0 && model.state == .done && !model.isError {
            ContentUnavailableView {
                Label("No rows", systemImage: "tray")
            } description: {
                Text("The statement finished in \(model.elapsedMs) ms and returned no rows.")
            }
        }
    }
}

/// An error is a first-class result, not a line of red text in the gutter.
struct ErrorCard: View {
    let message: String
    var onDismiss: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Image(systemName: "exclamationmark.octagon.fill")
                    .foregroundStyle(.red)
                Text("Query failed").font(.headline)
                Spacer(minLength: 12)
                Button { onDismiss() } label: { Image(systemName: "xmark") }
                    .buttonStyle(.borderless)
                    .foregroundStyle(.secondary)
            }
            ScrollView {
                Text(message)
                    .font(.system(size: 11, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 160)
        }
        .padding(14)
        .frame(maxWidth: 560, alignment: .leading)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .strokeBorder(Color.red.opacity(0.45), lineWidth: 1)
        )
        .shadow(color: .black.opacity(0.18), radius: 12, y: 4)
        .padding(20)
    }
}

// MARK: - shared geometry

enum Chrome {
    static let paneCorner: CGFloat = 8

    /// A recessed content pane: the editor and the grid both sit in one of
    /// these, so they read as two layers on the window rather than one flat
    /// plane of the same gray.
    static func pane<V: View>(_ content: V) -> some View {
        content
            .background(Color(nsColor: .textBackgroundColor))
            .clipShape(RoundedRectangle(cornerRadius: paneCorner, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: paneCorner, style: .continuous)
                    .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
            )
    }
}
