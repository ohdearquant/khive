// Probe: can this process use the Metal GPU? Prints one line per step and exits 0 only when a compute kernel ran
// on the device and returned the right numbers.
import Foundation
import Metal

func fail(_ step: String) -> Never {
    print("FAIL \(step)")
    exit(1)
}

guard let device = MTLCreateSystemDefaultDevice() else { fail("device: MTLCreateSystemDefaultDevice returned nil") }
print("device \(device.name)")
guard let queue = device.makeCommandQueue() else { fail("queue") }
print("queue ok")

let source = """
#include <metal_stdlib>
using namespace metal;
kernel void twice(device float *data [[buffer(0)]], uint id [[thread_position_in_grid]]) {
    data[id] = data[id] * 2.0;
}
"""
let library: MTLLibrary
do {
    library = try device.makeLibrary(source: source, options: nil)
} catch {
    fail("compile shader from source: \(error)")
}
print("shader compiled")
guard let function = library.makeFunction(name: "twice") else { fail("function") }
let pipeline: MTLComputePipelineState
do {
    pipeline = try device.makeComputePipelineState(function: function)
} catch {
    fail("pipeline: \(error)")
}
print("pipeline ok")

let count = 1024
var input = (0..<count).map { Float($0) }
guard let buffer = device.makeBuffer(bytes: &input, length: count * MemoryLayout<Float>.stride, options: .storageModeShared)
else { fail("buffer") }
guard let command = queue.makeCommandBuffer(), let encoder = command.makeComputeCommandEncoder() else { fail("encoder") }
encoder.setComputePipelineState(pipeline)
encoder.setBuffer(buffer, offset: 0, index: 0)
encoder.dispatchThreads(MTLSize(width: count, height: 1, depth: 1),
                        threadsPerThreadgroup: MTLSize(width: min(count, pipeline.maxTotalThreadsPerThreadgroup), height: 1, depth: 1))
encoder.endEncoding()
command.commit()
command.waitUntilCompleted()
if let error = command.error { fail("run: \(error)") }
let out = buffer.contents().bindMemory(to: Float.self, capacity: count)
guard out[1] == 2.0, out[1023] == 2046.0 else { fail("result: out[1]=\(out[1]) out[1023]=\(out[1023])") }
print("kernel ran on the GPU: out[1]=\(out[1]) out[1023]=\(out[1023])")
print("METAL-OK")
