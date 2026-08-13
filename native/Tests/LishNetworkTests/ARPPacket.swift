import Foundation

enum ARPPacket {
    static func request() -> Data {
        var arp = [UInt8](repeating: 0, count: 28)
        put16(1, in: &arp, at: 0)
        put16(0x0800, in: &arp, at: 2)
        arp[4] = 6
        arp[5] = 4
        put16(1, in: &arp, at: 6)
        arp.replaceSubrange(8..<14, with: DHCPPacket.clientMAC)
        arp.replaceSubrange(14..<18, with: [10, 0, 2, 15])
        arp.replaceSubrange(24..<28, with: [10, 0, 2, 2])
        return Data([UInt8](repeating: 0xff, count: 6) + DHCPPacket.clientMAC + [0x08, 0x06] + arp)
    }

    static func isReply(_ frame: Data) -> Bool {
        let bytes = [UInt8](frame)
        guard bytes.count >= 14 + 28,
              bytes[12] == 0x08, bytes[13] == 0x06,
              read16(bytes, at: 20) == 2,
              Array(bytes[28..<32]) == [10, 0, 2, 2],
              Array(bytes[38..<42]) == [10, 0, 2, 15] else { return false }
        return true
    }

    private static func put16(_ value: UInt16, in bytes: inout [UInt8], at offset: Int) {
        bytes[offset] = UInt8(truncatingIfNeeded: value >> 8)
        bytes[offset + 1] = UInt8(truncatingIfNeeded: value)
    }

    private static func read16(_ bytes: [UInt8], at offset: Int) -> UInt16 {
        UInt16(bytes[offset]) << 8 | UInt16(bytes[offset + 1])
    }
}
