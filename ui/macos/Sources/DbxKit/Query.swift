import CDbxFFI
import Foundation

public enum QueryState: String, Sendable {
    case streaming, parked, capped, done, cancelled, failed

    public var isTerminal: Bool {
        switch self {
        case .done, .cancelled, .failed, .capped: return true
        case .streaming, .parked: return false
        }
    }
}

public struct ColumnSpec: Sendable, Equatable {
    public let name: String
    public let type: String
}

public struct QueryStatus: Sendable {
    public let state: QueryState
    public let rowsLoaded: UInt64
    public let elapsedMs: UInt64
    public let error: String?
    public let columns: [ColumnSpec]
    public let totalKnown: Bool

    public static let empty = QueryStatus(
        state: .done, rowsLoaded: 0, elapsedMs: 0, error: nil, columns: [], totalKnown: true)
}

/// Retains the Swift closure the C progress callback trampolines into.
/// Kept alive by `DbxQueryHandle` for exactly as long as the query lives.
final class ProgressBox {
    let fire: () -> Void
    init(_ fire: @escaping () -> Void) { self.fire = fire }
}

private let progressTrampoline: @convention(c) (UnsafeMutableRawPointer?) -> Void = { ctx in
    guard let ctx else { return }
    Unmanaged<ProgressBox>.fromOpaque(ctx).takeUnretainedValue().fire()
}

/// Owns the `DbxQuery*`. `deinit` frees it, which (per the ABI contract, and as
/// implemented by both the stub and the real shim) joins the feeder first, so no
/// progress callback can outlive this object.
public final class DbxQueryHandle: @unchecked Sendable {
    let raw: OpaquePointer
    private var progressBox: ProgressBox?
    private let coalesceLock = NSLock()
    private var refreshInFlight = false

    init(raw: OpaquePointer) { self.raw = raw }

    deinit {
        dbx_query_free(raw)  // joins the background feeder BEFORE progressBox is released
        progressBox = nil
    }

    /// `handler` is always delivered on the main queue. The background callback
    /// itself is coalesced: while a hop is in flight, further callbacks are
    /// dropped, so a chatty feeder cannot flood the main queue with redraws.
    public func onProgress(_ handler: @escaping () -> Void) {
        let box = ProgressBox { [weak self] in
            guard let self else { return }
            self.coalesceLock.lock()
            let already = self.refreshInFlight
            self.refreshInFlight = true
            self.coalesceLock.unlock()
            if already { return }
            DispatchQueue.main.async {
                self.coalesceLock.lock()
                self.refreshInFlight = false
                self.coalesceLock.unlock()
                handler()
            }
        }
        progressBox = box
        dbx_query_on_progress(raw, progressTrampoline, Unmanaged.passUnretained(box).toOpaque())
    }

    public func status() throws -> QueryStatus {
        let json = try dbxTry { errOut in takeOwnedString(dbx_query_status_json(raw, errOut)) }
        guard let d = jsonObject(json) as? [String: Any] else {
            throw DbxError("unparseable status JSON: \(json)")
        }
        let cols = (d["columns"] as? [[String: Any]] ?? []).compactMap { c -> ColumnSpec? in
            guard let n = c["name"] as? String else { return nil }
            return ColumnSpec(name: n, type: c["type"] as? String ?? "")
        }
        return QueryStatus(
            state: QueryState(rawValue: d["state"] as? String ?? "failed") ?? .failed,
            rowsLoaded: (d["rows_loaded"] as? NSNumber)?.uint64Value ?? 0,
            elapsedMs: (d["elapsed_ms"] as? NSNumber)?.uint64Value ?? 0,
            error: d["error"] as? String,
            columns: cols,
            totalKnown: d["total_known"] as? Bool ?? false)
    }

    /// Returns instantly. The outcome JSON is shown to the user VERBATIM —
    /// for engines that cannot truly cancel it says so (design §3.3).
    public func cancel() -> String? {
        var outcome: UnsafeMutablePointer<CChar>?
        withUnsafeMutablePointer(to: &outcome) { dbx_query_cancel(raw, $0) }
        guard let text = takeOwnedString(outcome) else { return nil }
        if let d = jsonObject(text) as? [String: Any], let m = d["message"] as? String {
            return m
        }
        return text
    }

    /// Materialises exactly one window. The caller owns the returned object and
    /// the underlying `DbxRows*` dies with it.
    public func rows(offset: UInt64, len: UInt64) throws -> RowWindow {
        let ptr = try dbxTry { errOut in dbx_query_rows(raw, offset, len, errOut) }
        return RowWindow(raw: ptr, offset: offset)
    }
}
