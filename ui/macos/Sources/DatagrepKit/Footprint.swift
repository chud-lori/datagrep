import Darwin
import Foundation

public enum Footprint {
    public struct Sample {
        public let physFootprint: UInt64
        public let residentSize: UInt64
        public var physMB: Double { Double(physFootprint) / 1_048_576 }
        public var rssMB: Double { Double(residentSize) / 1_048_576 }
    }

    public static func sample() -> Sample {
        var info = task_vm_info_data_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size)
        let kr = withUnsafeMutablePointer(to: &info) { ptr in
            ptr.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { intPtr in
                task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), intPtr, &count)
            }
        }
        guard kr == KERN_SUCCESS else { return Sample(physFootprint: 0, residentSize: 0) }
        return Sample(
            physFootprint: UInt64(info.phys_footprint), residentSize: UInt64(info.resident_size))
    }

    /// Total CPU time consumed by every thread in this process, in seconds.
    public static func cpuSeconds() -> Double {
        var info = task_thread_times_info_data_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<task_thread_times_info_data_t>.size / MemoryLayout<natural_t>.size)
        let kr = withUnsafeMutablePointer(to: &info) { ptr in
            ptr.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { intPtr in
                task_info(mach_task_self_, task_flavor_t(TASK_THREAD_TIMES_INFO), intPtr, &count)
            }
        }
        guard kr == KERN_SUCCESS else { return 0 }
        let user = Double(info.user_time.seconds) + Double(info.user_time.microseconds) / 1e6
        let sys = Double(info.system_time.seconds) + Double(info.system_time.microseconds) / 1e6
        return user + sys
    }
}
