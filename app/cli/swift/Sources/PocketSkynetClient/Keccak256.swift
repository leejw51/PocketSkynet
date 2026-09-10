// Keccak-256 (the pre-NIST padding variant Ethereum uses, delimiter 0x01).
//
// Bundled rather than pulled from a dependency: the whole need is one digest
// over short messages, and the implementation is verified byte-exactly against
// the `eip191[]` vectors in `app/core/tests/vectors/protocol-v1.json`.

import Foundation

public enum Keccak256 {
    private static let roundConstants: [UInt64] = [
        0x0000_0000_0000_0001, 0x0000_0000_0000_8082, 0x8000_0000_0000_808a, 0x8000_0000_8000_8000,
        0x0000_0000_0000_808b, 0x0000_0000_8000_0001, 0x8000_0000_8000_8081, 0x8000_0000_0000_8009,
        0x0000_0000_0000_008a, 0x0000_0000_0000_0088, 0x0000_0000_8000_8009, 0x0000_0000_8000_000a,
        0x0000_0000_8000_808b, 0x8000_0000_0000_008b, 0x8000_0000_0000_8089, 0x8000_0000_0000_8003,
        0x8000_0000_0000_8002, 0x8000_0000_0000_0080, 0x0000_0000_0000_800a, 0x8000_0000_8000_000a,
        0x8000_0000_8000_8081, 0x8000_0000_0000_8080, 0x0000_0000_8000_0001, 0x8000_0000_8000_8008,
    ]

    /// Rotation offsets, in the lane order the pi step below walks.
    private static let rho: [UInt64] = [
        1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14,
        27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44,
    ]

    /// Lane permutation for the combined rho-and-pi step.
    private static let pi: [Int] = [
        10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4,
        15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1,
    ]

    private static let rate = 136 // bytes; 1600 bits minus 2x256-bit capacity

    private static func rotl(_ value: UInt64, _ by: UInt64) -> UInt64 {
        (value << by) | (value >> (64 - by))
    }

    private static func keccakF(_ state: inout [UInt64]) {
        var b = [UInt64](repeating: 0, count: 5)
        for round in 0..<24 {
            // Theta
            for x in 0..<5 {
                b[x] = state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20]
            }
            for x in 0..<5 {
                let d = b[(x + 4) % 5] ^ rotl(b[(x + 1) % 5], 1)
                for y in stride(from: 0, to: 25, by: 5) {
                    state[x + y] ^= d
                }
            }
            // Rho and pi
            var t = state[1]
            for i in 0..<24 {
                let j = pi[i]
                let saved = state[j]
                state[j] = rotl(t, rho[i])
                t = saved
            }
            // Chi
            for y in stride(from: 0, to: 25, by: 5) {
                for x in 0..<5 { b[x] = state[y + x] }
                for x in 0..<5 {
                    state[y + x] = b[x] ^ (~b[(x + 1) % 5] & b[(x + 2) % 5])
                }
            }
            // Iota
            state[0] ^= roundConstants[round]
        }
    }

    /// The 32-byte Keccak-256 digest of `input`.
    public static func hash(_ input: [UInt8]) -> [UInt8] {
        var state = [UInt64](repeating: 0, count: 25)

        // Absorb full blocks, then the padded final block.
        var block = [UInt8](repeating: 0, count: rate)
        var offset = 0
        while input.count - offset >= rate {
            for i in 0..<rate { block[i] = input[offset + i] }
            absorb(&state, block)
            offset += rate
        }
        var final = [UInt8](repeating: 0, count: rate)
        let remaining = input.count - offset
        for i in 0..<remaining { final[i] = input[offset + i] }
        final[remaining] ^= 0x01 // Keccak (not SHA-3) domain padding
        final[rate - 1] ^= 0x80
        absorb(&state, final)

        // Squeeze 32 bytes.
        var out = [UInt8]()
        out.reserveCapacity(32)
        for lane in 0..<4 {
            var v = state[lane].littleEndian
            withUnsafeBytes(of: &v) { out.append(contentsOf: $0) }
        }
        return out
    }

    public static func hash(_ input: Data) -> [UInt8] {
        hash([UInt8](input))
    }

    private static func absorb(_ state: inout [UInt64], _ block: [UInt8]) {
        for lane in 0..<(rate / 8) {
            var v: UInt64 = 0
            for i in (0..<8).reversed() {
                v = (v << 8) | UInt64(block[lane * 8 + i])
            }
            state[lane] ^= v
        }
        keccakF(&state)
    }
}
