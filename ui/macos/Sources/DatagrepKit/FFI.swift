import CDatagrepFFI
import Foundation

/// Every error the ABI can hand back, already copied out of C memory and freed.
public struct DatagrepError: Error, CustomStringConvertible {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

/// Copies a `char*` returned by the ABI into a Swift String and frees it.
@inline(__always)
func takeOwnedString(_ p: UnsafeMutablePointer<CChar>?) -> String? {
    guard let p else { return nil }
    defer { datagrep_string_free(p) }
    return String(cString: p)
}

@inline(__always)
func datagrepTry<T>(_ body: (UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> T?) throws -> T {
    var err: UnsafeMutablePointer<CChar>?
    let result = withUnsafeMutablePointer(to: &err) { body($0) }
    if let e = err {
        let message = String(cString: e)
        datagrep_string_free(e)
        throw DatagrepError(message)
    }
    guard let result else { throw DatagrepError("call failed without an error message") }
    return result
}

@inline(__always)
func datagrepTryBool(_ body: (UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Bool) throws {
    var err: UnsafeMutablePointer<CChar>?
    let ok = withUnsafeMutablePointer(to: &err) { body($0) }
    if let e = err {
        let message = String(cString: e)
        datagrep_string_free(e)
        throw DatagrepError(message)
    }
    if !ok { throw DatagrepError("call returned false without an error message") }
}

func jsonObject(_ text: String) -> Any? {
    guard let data = text.data(using: .utf8) else { return nil }
    return try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
}
