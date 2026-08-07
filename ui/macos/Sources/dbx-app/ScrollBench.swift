import AppKit
import DbxKit

/// A scripted fling through the grid, measuring the real cost of OUR path:
/// scroll -> NSTableView asks for the newly visible rows -> RowPager may fetch a
/// window through the FFI -> GridCellView draws -> the window backing store is
/// updated synchronously.
///
/// This is NOT a display-link sampler (a display link is a timer, which the
/// design bans). Each step runs one at a time on the main queue with the run
/// loop free in between, so the numbers are per-scroll-step frame times.
@MainActor
enum ScrollBench {
    static var isRunning = false

    static func run(on results: ResultsViewController, model: AppModel, steps: Int = 400) {
        guard !isRunning else { return }
        let table = results.tableView
        let total = table.numberOfRows
        guard total > 100 else {
            FileHandle.standardError.write(Data("BENCH skipped: only \(total) rows\n".utf8))
            return
        }
        isRunning = true
        var samples: [Double] = []
        samples.reserveCapacity(steps)
        let before = Footprint.sample()
        let cpuBefore = Footprint.cpuSeconds()
        // 37 rows/step: deliberately not a multiple of the 512-row page size, so
        // page boundaries land mid-viewport rather than aligning conveniently.
        let stride = 37

        func step(_ i: Int) {
            if i >= steps {
                finish(samples, before: before, cpuBefore: cpuBefore, results: results,
                       model: model)
                return
            }
            let row = (i * stride) % max(total - 60, 1)
            let t0 = DispatchTime.now().uptimeNanoseconds
            table.scrollRowToVisible(min(row + 45, total - 1))
            table.scrollRowToVisible(row)
            table.layoutSubtreeIfNeeded()
            table.displayIfNeeded()
            table.window?.displayIfNeeded()
            let dt = Double(DispatchTime.now().uptimeNanoseconds - t0) / 1e6
            samples.append(dt)
            DispatchQueue.main.async { step(i + 1) }
        }
        FileHandle.standardError.write(
            Data("BENCH starting scroll fling: \(steps) steps over \(total) rows\n".utf8))
        DispatchQueue.main.async { step(0) }
    }

    private static func finish(
        _ samples: [Double], before: Footprint.Sample, cpuBefore: Double,
        results: ResultsViewController, model: AppModel
    ) {
        isRunning = false
        let sorted = samples.sorted()
        func pct(_ p: Double) -> Double {
            guard !sorted.isEmpty else { return 0 }
            let i = min(sorted.count - 1, max(0, Int((p / 100) * Double(sorted.count)) ))
            return sorted[i]
        }
        let after = Footprint.sample()
        let cpu = Footprint.cpuSeconds() - cpuBefore
        let pager = results.pager
        let report = String(
            format: """
                BENCH scroll frame time over %d steps (ms): p50 %.2f  p95 %.2f  p99 %.2f  max %.2f  min %.2f
                BENCH rows in grid: %d
                BENCH phys_footprint before %.1f MB -> after %.1f MB (rss %.1f -> %.1f)
                BENCH cpu during fling: %.3f s
                BENCH pager: %llu fetches, avg %.3f ms, max %.3f ms, %llu evictions, resident %d pages / %llu rows
                """,
            samples.count, pct(50), pct(95), pct(99), sorted.last ?? 0, sorted.first ?? 0,
            results.tableView.numberOfRows, before.physMB, after.physMB, before.rssMB, after.rssMB,
            cpu, pager?.fetches ?? 0,
            Double(pager?.totalFetchNanos ?? 0) / Double(max(pager?.fetches ?? 1, 1)) / 1e6,
            Double(pager?.maxFetchNanos ?? 0) / 1e6, pager?.evictions ?? 0,
            pager?.residentPages ?? 0, pager?.residentRows ?? 0)
        FileHandle.standardError.write(Data((report + "\n").utf8))
        model.message = String(
            format: "scroll p50/p95/p99 = %.2f / %.2f / %.2f ms", pct(50), pct(95), pct(99))
        model.isError = false
        model.refreshFootprint()

        if ProcessInfo.processInfo.arguments.contains("--quit-after-bench") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { NSApp.terminate(nil) }
        }
    }
}
