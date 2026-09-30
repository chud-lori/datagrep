import AppKit
import DatagrepKit
import UniformTypeIdentifiers

struct ExportChoice {
    let url: URL
    let format: ExportFormat
    let table: String?
}

struct ExportRequest {
    let profile: String
    let sql: String
    let choice: ExportChoice
}

/// The save panel, with the format picker and — for INSERT statements — the table they target.
@MainActor
final class ExportPanel: NSObject, NSOpenSavePanelDelegate {
    private let panel = NSSavePanel()
    private let formats: [ExportFormat]
    private let picker = NSPopUpButton()
    private let tableLabel = NSTextField(labelWithString: "Table:")
    private let table = NSTextField()
    // The panel holds its delegate weakly; this keeps it alive while the sheet is up.
    private static var open: ExportPanel?

    static func present(
        formats: [ExportFormat], suggestedName: String, done: @escaping (ExportChoice?) -> Void
    ) {
        let box = ExportPanel(formats: formats, name: suggestedName)
        open = box
        let finish: (NSApplication.ModalResponse) -> Void = { response in
            open = nil
            guard response == .OK, let url = box.panel.url else { return done(nil) }
            let format = box.selected
            let table = box.table.stringValue.trimmingCharacters(in: .whitespaces)
            done(ExportChoice(url: url, format: format, table: format == .sql ? table : nil))
        }
        if let window = NSApp.keyWindow {
            box.panel.beginSheetModal(for: window, completionHandler: finish)
        } else {
            finish(box.panel.runModal())
        }
    }

    private init(formats: [ExportFormat], name: String) {
        self.formats = formats.isEmpty ? [.csv] : formats
        super.init()
        panel.title = "Export Result"
        panel.prompt = "Export"
        panel.message = "Runs the statement again and writes every row, not just the loaded ones."
        panel.canCreateDirectories = true
        panel.delegate = self

        for format in self.formats { picker.addItem(withTitle: format.title) }
        picker.target = self
        picker.action = #selector(formatChanged)
        table.placeholderString = "schema.table"
        table.widthAnchor.constraint(equalToConstant: 180).isActive = true

        let row = NSStackView(views: [
            NSTextField(labelWithString: "Format:"), picker, tableLabel, table,
        ])
        row.spacing = 8
        row.edgeInsets = NSEdgeInsets(top: 10, left: 16, bottom: 10, right: 16)
        panel.accessoryView = row

        panel.nameFieldStringValue = name
        formatChanged()
    }

    private var selected: ExportFormat {
        formats[max(0, min(picker.indexOfSelectedItem, formats.count - 1))]
    }

    @objc private func formatChanged() {
        let format = selected
        let needsTable = format == .sql
        tableLabel.isHidden = !needsTable
        table.isHidden = !needsTable
        panel.allowedContentTypes = [UTType(filenameExtension: format.fileExtension) ?? .plainText]
        let stem = (panel.nameFieldStringValue as NSString).deletingPathExtension
        panel.nameFieldStringValue = "\(stem.isEmpty ? "export" : stem).\(format.fileExtension)"
    }

    func panel(_ sender: Any, validate url: URL) throws {
        guard selected == .sql, table.stringValue.trimmingCharacters(in: .whitespaces).isEmpty
        else { return }
        throw NSError(
            domain: "datagrep.export", code: 1,
            userInfo: [
                NSLocalizedDescriptionKey: "Name the table the INSERT statements should target."
            ])
    }
}
