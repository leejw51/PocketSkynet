import Foundation

public enum Hex {
    /// Lowercase hex, no prefix.
    public static func encode(_ bytes: [UInt8]) -> String {
        bytes.map { String(format: "%02x", $0) }.joined()
    }

    public static func encode(_ data: Data) -> String {
        encode([UInt8](data))
    }

    /// Decode hex (with or without `0x`), accepting either case.
    /// Returns `nil` for odd length or non-hex characters.
    public static func decode(_ string: String) -> [UInt8]? {
        var s = Substring(string)
        if s.hasPrefix("0x") || s.hasPrefix("0X") {
            s = s.dropFirst(2)
        }
        guard s.count % 2 == 0 else { return nil }
        var bytes = [UInt8]()
        bytes.reserveCapacity(s.count / 2)
        var index = s.startIndex
        while index < s.endIndex {
            let next = s.index(index, offsetBy: 2)
            guard let byte = UInt8(s[index..<next], radix: 16) else { return nil }
            bytes.append(byte)
            index = next
        }
        return bytes
    }
}
