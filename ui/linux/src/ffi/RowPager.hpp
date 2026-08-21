// RowPager.hpp — a bounded, page-keyed LRU over dg::RowWindow.

#ifndef DATAGREP_ROW_PAGER_HPP
#define DATAGREP_ROW_PAGER_HPP

#include "DatagrepFfi.hpp"

#include <cstdint>
#include <deque>
#include <unordered_map>

namespace dg {

class RowPager {
public:
    // maxPages is clamped >= 1 so eviction can never free the page just inserted.
    explicit RowPager(const Query& query, std::uint64_t pageSize = 512,
                      int maxPages = 4)
        : query_(query),
          pageSize_(pageSize == 0 ? 1 : pageSize),
          maxPages_(maxPages < 1 ? 1 : maxPages) {}

    std::uint64_t pageSize() const { return pageSize_; }

    void invalidateAll() {
        pages_.clear();
        order_.clear();
    }

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
