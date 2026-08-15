// DatagrepFfi.hpp — a thin, RAII C++ wrapper over the datagrep C ABI.
//
// This is the Linux analogue of DatagrepKit's FFI.swift / Core.swift /
// Query.swift / RowWindow.swift. It is deliberately dependency-free (only the
// C++17 standard library) so that the ABI layer stays honest and testable in
// isolation; all Qt/JSON glue lives one level up in the model.
//
// The C ABI (crates/datagrep-ffi/include/datagrep.h) is the ONLY interface this
// code may call. Its ownership rules are frozen and are mirrored here 1:1:
//
//   * Every OWNED `char*` the ABI returns (by value or via a `char** err_out` /
//     `char** *_json_out` out-param) MUST be released with
//     datagrep_string_free(). `takeOwnedString()` is the single choke point for
//     that — nothing else in this codebase is allowed to hold a raw owned
//     char* past a statement boundary.
//
//   * datagrep_rows_cell() returns a BORROWED, NOT-nul-terminated `const char*`
//     that points into the row window's arena and is valid only until the
//     owning DatagrepRows is freed. It MUST NOT be passed to
//     datagrep_string_free() — doing so corrupts the heap. RowWindow::cellText()
//     copies it out by (ptr,len) and never frees it. The `const` on the return
//     type is the ABI's signal: only an owned `char*` is freeable.
//
//   * Every DatagrepCore*, DatagrepQuery* and DatagrepRows* MUST be released
//     with its matching _free(). Each is owned by exactly one RAII class below
//     and freed exactly once, in that class's destructor.
//
// The engine is never allowed to change to suit the UI; where the shipped header
// lacks an `extern "C"` guard we add it around the include here rather than
// editing the crate. See datagrep_c_abi.h.

#ifndef DATAGREP_FFI_HPP
#define DATAGREP_FFI_HPP

#include "datagrep_c_abi.h"  // includes crates/datagrep-ffi/include/datagrep.h under extern "C"

#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <utility>

namespace dg {

// Every error the ABI hands back, already copied out of C memory and freed.
// Mirrors DatagrepKit.DatagrepError.
class Error : public std::runtime_error {
public:
    explicit Error(std::string message) : std::runtime_error(std::move(message)) {}
};

namespace detail {

// Copies an owned `char*` returned by the ABI into a std::string and frees it
// with datagrep_string_free(). A null pointer becomes std::nullopt. This is the
// ONLY place an owned char* is consumed — see the header contract above.
inline std::optional<std::string> takeOwnedString(char* p) {
    if (p == nullptr) {
        return std::nullopt;
    }
    std::string out(p);  // copies up to the NUL; owned strings are nul-terminated
    datagrep_string_free(p);
    return out;
}

// Runs an ABI call that uses the `char** err_out` convention and returns an
// owned value pointer (e.g. char* or DatagrepQuery*). The error string is copied
// and freed on every path, so it can never leak. Throws dg::Error if the ABI set
// err_out; throws if it returned null without an error (the ABI's "failed
// without a message" case). Mirrors DatagrepKit.datagrepTry.
template <typename Fn>
auto tryCall(Fn&& body) -> std::remove_pointer_t<decltype(body(std::declval<char**>()))>* {
    char* err = nullptr;
    auto* result = body(&err);
    if (err != nullptr) {
        std::string message(err);
        datagrep_string_free(err);
        throw Error(std::move(message));
    }
    if (result == nullptr) {
        throw Error("datagrep call failed without an error message");
    }
    return result;
}

// The `char** err_out` convention for a call that returns bool (add/remove/
// update). Mirrors DatagrepKit.datagrepTryBool.
template <typename Fn>
void tryCallBool(Fn&& body) {
    char* err = nullptr;
    bool ok = body(&err);
    if (err != nullptr) {
        std::string message(err);
        datagrep_string_free(err);
        throw Error(std::move(message));
    }
    if (!ok) {
        throw Error("datagrep call returned false without an error message");
    }
}

// Same as tryCall, but for calls that return an owned `char*` where a null
// return with no error is legitimately "nothing" rather than a failure (only
// used for status/detail JSON, which the ABI always populates on success).
template <typename Fn>
std::string tryCallJson(Fn&& body) {
    char* err = nullptr;
    char* result = body(&err);
    if (err != nullptr) {
        std::string message(err);
        datagrep_string_free(err);
        throw Error(std::move(message));
    }
    auto s = takeOwnedString(result);
    if (!s) {
        throw Error("datagrep call returned no JSON and no error");
    }
    return *s;
}

}  // namespace detail

// The four facts a cell can carry, matching datagrep_rows_cell_kind():
//   value  — a real value; cellText() is its rendering.
//   null   — SQL NULL. Distinct from Absent.
//   absent — the field is not present in this document at all (Mongo/ES).
//   nested — a document/array; cellText() is a summary like "{3 fields}".
enum class CellKind : std::uint8_t {
    Value = 0,
    Null = 1,
    Absent = 2,
    Nested = 3,
};

inline CellKind cellKindFromRaw(std::uint8_t raw) {
    switch (raw) {
        case 1: return CellKind::Null;
        case 2: return CellKind::Absent;
        case 3: return CellKind::Nested;
        default: return CellKind::Value;
    }
}

// Owns one DatagrepRows* — a single materialised [offset, offset+len) window.
// Freed exactly once, in the destructor, which is what makes LRU eviction in the
// pager leak-proof by construction. Mirrors DatagrepKit.RowWindow.
//
// Move-only: copying would double-free the DatagrepRows*.
class RowWindow {
public:
    RowWindow(DatagrepRows* raw, std::uint64_t offset)
        : raw_(raw),
          offset_(offset),
          count_(datagrep_rows_count(raw)),
          columns_(datagrep_rows_columns(raw)),
          pending_(datagrep_rows_pending(raw)) {}

    ~RowWindow() {
        if (raw_ != nullptr) {
            datagrep_rows_free(raw_);
        }
    }

    RowWindow(const RowWindow&) = delete;
    RowWindow& operator=(const RowWindow&) = delete;

    RowWindow(RowWindow&& other) noexcept
        : raw_(other.raw_),
          offset_(other.offset_),
          count_(other.count_),
          columns_(other.columns_),
          pending_(other.pending_) {
        other.raw_ = nullptr;
    }
    RowWindow& operator=(RowWindow&& other) noexcept {
        if (this != &other) {
            if (raw_ != nullptr) {
                datagrep_rows_free(raw_);
            }
            raw_ = other.raw_;
            offset_ = other.offset_;
            count_ = other.count_;
            columns_ = other.columns_;
            pending_ = other.pending_;
            other.raw_ = nullptr;
        }
        return *this;
    }

    std::uint64_t offset() const { return offset_; }
    std::uint64_t count() const { return count_; }   // rows actually in this window
    std::uint32_t columns() const { return columns_; }
    bool pending() const { return pending_; }        // true => draw skeletons

    bool contains(std::uint64_t absoluteRow) const {
        return absoluteRow >= offset_ && absoluteRow < offset_ + count_;
    }

    CellKind kind(std::uint64_t absoluteRow, std::uint32_t col) const {
        return cellKindFromRaw(
            datagrep_rows_cell_kind(raw_, absoluteRow - offset_, col));
    }

    // Copies the borrowed, NOT-nul-terminated cell text out by (ptr,len). The
    // pointer is NEVER freed — it lives in this window's arena and dies with it.
    // Called only for on-screen cells, never for a whole window.
    std::string cellText(std::uint64_t absoluteRow, std::uint32_t col) const {
        std::size_t len = 0;
        const char* p = datagrep_rows_cell(raw_, absoluteRow - offset_, col, &len);
        if (p == nullptr || len == 0) {
            return std::string();
        }
        return std::string(p, len);  // COPY. Do not free p.
    }

    // Full raw value of one cell as owned JSON, for the detail pane. Freed here.
    std::optional<std::string> cellDetailJson(std::uint64_t absoluteRow,
                                              std::uint32_t col) const {
        return detail::takeOwnedString(
            datagrep_rows_cell_detail_json(raw_, absoluteRow - offset_, col));
    }

private:
    DatagrepRows* raw_;
    std::uint64_t offset_;
    std::uint64_t count_;
    std::uint32_t columns_;
    bool pending_;
};

// Owns the DatagrepQuery*. The destructor frees it, which (per the ABI contract)
// joins the background feeder BEFORE the progress callback storage is destroyed,
// so no callback can outlive this object. Mirrors DatagrepKit.DatagrepQueryHandle.
//
// Move-only. The progress std::function is heap-stable (unique_ptr) so its
// address — handed to the C ABI as the callback ctx — survives moves of the
// wrapper.
class Query {
public:
    explicit Query(DatagrepQuery* raw) : raw_(raw) {}

    ~Query() {
        if (raw_ != nullptr) {
            // Frees the query AND joins the feeder before progress_ is destroyed
            // below, exactly matching the ABI's ordering guarantee.
            datagrep_query_free(raw_);
        }
        // progress_ (if any) is destroyed after the join — safe.
    }

    Query(const Query&) = delete;
    Query& operator=(const Query&) = delete;

    Query(Query&& other) noexcept
        : raw_(other.raw_), progress_(std::move(other.progress_)) {
        other.raw_ = nullptr;
    }
    Query& operator=(Query&& other) noexcept {
        if (this != &other) {
            if (raw_ != nullptr) {
                datagrep_query_free(raw_);
            }
            raw_ = other.raw_;
            progress_ = std::move(other.progress_);
            other.raw_ = nullptr;
        }
        return *this;
    }

    // Status snapshot as raw JSON (parsed one level up). The ABI's contract:
    // {"state":..,"rows_loaded":u64,"affected_rows":u64|null,"elapsed_ms":u64,
    //  "error":str|null,"read_only":..,"columns":[{"name","type"}],
    //  "total_known":bool}
    std::string statusJson() const {
        return detail::tryCallJson(
            [&](char** err) { return datagrep_query_status_json(raw_, err); });
    }

    // Cancel. Always returns instantly. Returns the SERVER's outcome as raw JSON
    // (or std::nullopt when the ABI reported none) — the caller shows it to the
    // user verbatim, because for engines that cannot truly cancel it says so.
    std::optional<std::string> cancel() {
        char* outcome = nullptr;
        datagrep_query_cancel(raw_, &outcome);
        return detail::takeOwnedString(outcome);
    }

    // Registers a progress callback. IMPORTANT: the ABI fires it on a BACKGROUND
    // THREAD — the caller's closure MUST marshal to the GUI thread itself (the
    // model does this with a queued Qt signal). The closure is stored heap-stable
    // and kept alive for exactly as long as this Query lives.
    void onProgress(std::function<void()> handler) {
        auto next = std::make_unique<std::function<void()>>(std::move(handler));
        // Register the NEW ctx before releasing the old one. The ABI swaps the
        // (cb, ctx) pair under the same lock it holds while firing, so once this
        // call returns no in-flight callback can still be using the old closure —
        // only then is destroying it safe. Assigning progress_ first would open a
        // window where the feeder fires into freed memory.
        datagrep_query_on_progress(raw_, &Query::trampoline, next.get());
        progress_ = std::move(next);
    }

    // Materialises exactly one window [offset, offset+len). The returned RowWindow
    // owns the underlying DatagrepRows* and frees it on destruction.
    RowWindow rows(std::uint64_t offset, std::uint64_t len) const {
        DatagrepRows* ptr = detail::tryCall(
            [&](char** err) { return datagrep_query_rows(raw_, offset, len, err); });
        return RowWindow(ptr, offset);
    }

private:
    static void trampoline(void* ctx) {
        if (ctx == nullptr) {
            return;
        }
        // Fired on a background thread. The stored closure is responsible for the
        // thread hop; we only invoke it.
        (*static_cast<std::function<void()>*>(ctx))();
    }

    DatagrepQuery* raw_;
    std::unique_ptr<std::function<void()>> progress_;
};

// Owns the DatagrepCore* (engine + its own tokio runtime thread). Freed exactly
// once, in the destructor. Mirrors DatagrepKit.DatagrepCoreHandle. All methods
// are thin pass-throughs to the ABI: this class holds NO business logic.
class Core {
public:
    explicit Core(const std::string& profilesDbPath) {
        raw_ = detail::tryCall([&](char** err) {
            return datagrep_core_new(profilesDbPath.c_str(), err);
        });
    }

    ~Core() {
        if (raw_ != nullptr) {
            datagrep_core_free(raw_);
        }
    }

    Core(const Core&) = delete;
    Core& operator=(const Core&) = delete;
    Core(Core&& other) noexcept : raw_(other.raw_) { other.raw_ = nullptr; }
    Core& operator=(Core&& other) noexcept {
        if (this != &other) {
            if (raw_ != nullptr) {
                datagrep_core_free(raw_);
            }
            raw_ = other.raw_;
            other.raw_ = nullptr;
        }
        return *this;
    }

    // --- profiles (raw JSON in / out; parsed one level up) -----------------
    std::string profilesListJson() const {
        return detail::tryCallJson(
            [&](char** err) { return datagrep_profiles_list_json(raw_, err); });
    }

    std::string profileGetJson(const std::string& name) const {
        return detail::tryCallJson([&](char** err) {
            return datagrep_profiles_get_json(raw_, name.c_str(), err);
        });
    }

    std::string connectionInfoJson(const std::string& name) const {
        return detail::tryCallJson([&](char** err) {
            return datagrep_connection_info_json(raw_, name.c_str(), err);
        });
    }

    void addProfile(const std::string& name, const std::string& url) {
        detail::tryCallBool([&](char** err) {
            return datagrep_profiles_add(raw_, name.c_str(), url.c_str(), err);
        });
    }

    // options_json may be empty ("" is accepted by the ABI as "defaults").
    void addProfileJson(const std::string& name, const std::string& url,
                        const std::string& optionsJson) {
        detail::tryCallBool([&](char** err) {
            return datagrep_profiles_add_json(raw_, name.c_str(), url.c_str(),
                                              optionsJson.c_str(), err);
        });
    }

    void updateProfile(const std::string& name, const std::string& patchJson) {
        detail::tryCallBool([&](char** err) {
            return datagrep_profiles_update(raw_, name.c_str(), patchJson.c_str(),
                                            err);
        });
    }

    void removeProfile(const std::string& name) {
        detail::tryCallBool([&](char** err) {
            return datagrep_profiles_remove(raw_, name.c_str(), err);
        });
    }

    // --- catalog (lazy, ONE level per call) --------------------------------
    // pathJson is a JSON array of segments, e.g. ["main"] or "[]" for roots.
    std::string catalogChildrenJson(const std::string& profile,
                                    const std::string& pathJson) const {
        return detail::tryCallJson([&](char** err) {
            return datagrep_catalog_children_json(raw_, profile.c_str(),
                                                  pathJson.c_str(), err);
        });
    }

    std::string catalogDescribeJson(const std::string& profile,
                                    const std::string& pathJson) const {
        return detail::tryCallJson([&](char** err) {
            return datagrep_catalog_describe_json(raw_, profile.c_str(),
                                                  pathJson.c_str(), err);
        });
    }

    // --- query -------------------------------------------------------------
    // Non-blocking: returns immediately with a handle; rows stream in the
    // background. The returned Query owns the DatagrepQuery*.
    Query run(const std::string& profile, const std::string& sql) const {
        DatagrepQuery* ptr = detail::tryCall([&](char** err) {
            return datagrep_query_run(raw_, profile.c_str(), sql.c_str(), err);
        });
        return Query(ptr);
    }

private:
    DatagrepCore* raw_ = nullptr;
};

}  // namespace dg

#endif  // DATAGREP_FFI_HPP
