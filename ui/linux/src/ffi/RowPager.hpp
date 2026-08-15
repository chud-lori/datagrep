// RowPager.hpp — a bounded, page-keyed LRU over dg::RowWindow.
//
// This is the whole memory story of the grid: the model may report 1,000,000
// rows, but at most `maxPages * pageSize` rows are ever materialised at once, and
// evicting a page drops its DatagrepRows immediately (dg::RowWindow's destructor
// calls datagrep_rows_free). Mirrors DatagrepKit.RowPager.
//
// Dependency-free (std only): the pager is pure ABI plumbing with no Qt.

#ifndef DATAGREP_ROW_PAGER_HPP
#define DATAGREP_ROW_PAGER_HPP

#include "DatagrepFfi.hpp"

#include <cstdint>
#include <deque>
#include <unordered_map>

namespace dg {

class RowPager {
public:
    // 512-row pages, 4 pages resident => at most 2,048 rows materialised, exactly
    // as the macOS grid is tuned. `query` must outlive the pager.
    // maxPages is clamped to >= 1: the eviction loop in window() must never be
    // able to evict the page it just inserted, or the returned pointer would
    // dangle. (pageSize is likewise kept >= 1 so page arithmetic can't divide
    // by zero.)
    explicit RowPager(const Query& query, std::uint64_t pageSize = 512,
                      int maxPages = 4)
        : query_(query),
          pageSize_(pageSize == 0 ? 1 : pageSize),
          maxPages_(maxPages < 1 ? 1 : maxPages) {}

    std::uint64_t pageSize() const { return pageSize_; }

    // Drops every cached page. Each dg::RowWindow destructor frees its
    // DatagrepRows. Call when a brand-new result replaces the old one.
    void invalidateAll() {
        pages_.clear();
        order_.clear();
    }

    // Drops cached pages that were only partially filled while streaming (a
    // half-page fetched at 40% progress), so it is re-fetched once more rows
    // land. Fully-materialised pages are kept. Mirrors invalidatePartialPages().
    void invalidatePartialPages() {
        for (auto it = pages_.begin(); it != pages_.end();) {
            const RowWindow& w = it->second;
            if (w.pending() || w.count() < pageSize_) {
                std::uint64_t key = it->first;
                it = pages_.erase(it);
                eraseFromOrder(key);
            } else {
                ++it;
            }
        }
    }

    // Returns the window that contains `absoluteRow`, fetching and caching its
    // page on a miss. Returns nullptr if the row is not (yet) available — the
    // caller draws a skeleton/blank cell. Never materialises more than one page
    // per miss and never touches rows outside [page, page+pageSize).
    const RowWindow* window(std::uint64_t absoluteRow) {
        const std::uint64_t page = absoluteRow / pageSize_;
        auto it = pages_.find(page);
        if (it != pages_.end()) {
            touch(page);
            return it->second.contains(absoluteRow) ? &it->second : nullptr;
        }
        RowWindow w = query_.rows(page * pageSize_, pageSize_);
        auto [inserted, ok] = pages_.emplace(page, std::move(w));
        order_.push_back(page);
        while (static_cast<int>(order_.size()) > maxPages_) {
            std::uint64_t victim = order_.front();
            order_.pop_front();
            pages_.erase(victim);  // datagrep_rows_free happens here
        }
        const RowWindow& win = inserted->second;
        return win.contains(absoluteRow) ? &win : nullptr;
    }

    std::uint64_t residentRows() const {
        std::uint64_t total = 0;
        for (const auto& [key, w] : pages_) {
            total += w.count();
        }
        return total;
    }
    int residentPages() const { return static_cast<int>(pages_.size()); }

private:
    void touch(std::uint64_t page) {
        eraseFromOrder(page);
        order_.push_back(page);
    }
    void eraseFromOrder(std::uint64_t page) {
        for (auto it = order_.begin(); it != order_.end(); ++it) {
            if (*it == page) {
                order_.erase(it);
                return;
            }
        }
    }

    const Query& query_;
    std::uint64_t pageSize_;
    int maxPages_;
    std::unordered_map<std::uint64_t, RowWindow> pages_;  // page index -> window
    std::deque<std::uint64_t> order_;  // least-recently-used at the front
};

}  // namespace dg

#endif  // DATAGREP_ROW_PAGER_HPP
