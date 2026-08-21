// DatagrepFfi.hpp — a thin, RAII C++ wrapper over the datagrep C ABI.

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
class Error : public std::runtime_error {
public:
    explicit Error(std::string message) : std::runtime_error(std::move(message)) {}
};

namespace detail {

// The only place an owned char* from the ABI is consumed and freed.
inline std::optional<std::string> takeOwnedString(char* p) {
    if (p == nullptr) {
        return std::nullopt;
    }
    std::string out(p);  // copies up to the NUL; owned strings are nul-terminated
    datagrep_string_free(p);
    return out;
}

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

    // Borrowed, NOT nul-terminated (ptr,len); never pass to datagrep_string_free().
    std::string cellText(std::uint64_t absoluteRow, std::uint32_t col) const {
        std::size_t len = 0;
        const char* p = datagrep_rows_cell(raw_, absoluteRow - offset_, col, &len);
        if (p == nullptr || len == 0) {
            return std::string();
        }
        return std::string(p, len);  // COPY. Do not free p.
    }

    // The field names THIS window projected, in column order, as owned JSON.
    std::optional<std::string> columnNamesJson() const {
        return detail::takeOwnedString(datagrep_rows_column_names_json(raw_));
    }

    // Full raw value of one cell as owned JSON, for the detail pane. Freed here.
    std::optional<std::string> cellDetailJson(std::uint64_t absoluteRow,
                                              std::uint32_t col) const {
        return detail::takeOwnedString(
            datagrep_rows_cell_detail_json(raw_, absoluteRow - offset_, col));
    }

    std::optional<std::string> envelopeJson(std::uint64_t absoluteRow) const {
        return detail::takeOwnedString(
            datagrep_rows_envelope_json(raw_, absoluteRow - offset_));
    }

private:
    DatagrepRows* raw_;
    std::uint64_t offset_;
    std::uint64_t count_;
    std::uint32_t columns_;
    bool pending_;
};

class Query {
public:
    explicit Query(DatagrepQuery* raw) : raw_(raw) {}

    ~Query() {
        if (raw_ != nullptr) {
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

    std::string statusJson() const {
        return detail::tryCallJson(
            [&](char** err) { return datagrep_query_status_json(raw_, err); });
    }

    std::optional<std::string> cancel() {
        char* outcome = nullptr;
        datagrep_query_cancel(raw_, &outcome);
        return detail::takeOwnedString(outcome);
    }

    // The function is heap-stable: its address is the C callback ctx and must survive moves.
    void onProgress(std::function<void()> handler) {
        auto next = std::make_unique<std::function<void()>>(std::move(handler));
        datagrep_query_on_progress(raw_, &Query::trampoline, next.get());
        progress_ = std::move(next);
    }

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
        (*static_cast<std::function<void()>*>(ctx))();
    }

    DatagrepQuery* raw_;
    std::unique_ptr<std::function<void()>> progress_;
};

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

    // Blocks for up to the engine's connect timeout — call off the GUI thread.
    std::string connectionTestJson(const std::string& name,
                                   const std::string& url) const {
        return detail::tryCallJson([&](char** err) {
            return datagrep_connection_test_json(raw_, name.c_str(), url.c_str(),
                                                 err);
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

    std::string mutateJson(const std::string& profile,
                           const std::string& mutationJson) const {
        return detail::tryCallJson([&](char** err) {
            return datagrep_mutate(raw_, profile.c_str(), mutationJson.c_str(), err);
        });
    }

    std::string rereadDocumentsJson(const std::string& profile,
                                    const std::string& addressesJson) const {
        return detail::tryCallJson([&](char** err) {
            return datagrep_reread_documents(raw_, profile.c_str(),
                                             addressesJson.c_str(), err);
        });
    }

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
