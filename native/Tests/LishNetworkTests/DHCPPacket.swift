import Foundation

enum DHCPPacket {
    static let clientMAC: [UInt8] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
    static let transactionID: UInt32 = 0x4c495348

    static func discover() -> Data {
        var bootp = [UInt8](repeating: 0, count: 240)
        bootp[0] = 1
        bootp[1] = 1
        bootp[2] = 6
        put32(transactionID, in: &bootp, at: 4)
        bootp[10] = 0x80
        bootp.replaceSubrange(28..<34, with: clientMAC)
        bootp[236...239] = [99, 130, 83, 99]
        bootp += [53, 1, 1]
        bootp += [55, 4, 1, 3, 6, 15]
        bootp += [255]

        var udp = [UInt8](repeating: 0, count: 8)
        put16(68, in: &udp, at: 0)
        put16(67, in: &udp, at: 2)
        put16(UInt16(udp.count + bootp.count), in: &udp, at: 4)

        var ip = [UInt8](repeating: 0, count: 20)
        ip[0] = 0x45
        put16(UInt16(ip.count + udp.count + bootp.count), in: &ip, at: 2)
        put16(0x4000, in: &ip, at: 6)
        ip[8] = 64
        ip[9] = 17
        ip[16...19] = [255, 255, 255, 255]
        put16(checksum(ip), in: &ip, at: 10)

        var ethernet = [UInt8](repeating: 0xff, count: 6)
        ethernet += clientMAC
        ethernet += [0x08, 0x00]
        ethernet += ip
        ethernet += udp
        ethernet += bootp
        return Data(ethernet)
    }

    static func isOffer(_ frame: Data) -> Bool {
        let bytes = [UInt8](frame)
        guard bytes.count >= 14 + 20 + 8 + 240,
              bytes[12] == 0x08, bytes[13] == 0x00 else { return false }
        let ipOffset = 14
        let ipLength = Int(bytes[ipOffset] & 0x0f) * 4
        guard ipLength >= 20, bytes.count >= ipOffset + ipLength + 8 + 240,
              bytes[ipOffset + 9] == 17 else { return false }
        let udpOffset = ipOffset + ipLength
        guard read16(bytes, at: udpOffset) == 67,
              read16(bytes, at: udpOffset + 2) == 68 else { return false }
        let bootpOffset = udpOffset + 8
        guard read32(bytes, at: bootpOffset + 4) == transactionID,
              Array(bytes[(bootpOffset + 16)..<(bootpOffset + 20)]) == [10, 0, 2, 15],
              Array(bytes[(bootpOffset + 236)..<(bootpOffset + 240)]) == [99, 130, 83, 99]
        else { return false }

        var offset = bootpOffset + 240
        while offset < bytes.count {
            let option = bytes[offset]
            offset += 1
            if option == 0 { continue }
            if option == 255 { break }
            guard offset < bytes.count else { return false }
            let length = Int(bytes[offset])
            offset += 1
            guard offset + length <= bytes.count else { return false }
            if option == 53, length == 1 { return bytes[offset] == 2 }
            offset += length
        }
        return false
    }

    private static func put16(_ value: UInt16, in bytes: inout [UInt8], at offset: Int) {
        bytes[offset] = UInt8(truncatingIfNeeded: value >> 8)
        bytes[offset + 1] = UInt8(truncatingIfNeeded: value)
    }

    private static func put32(_ value: UInt32, in bytes: inout [UInt8], at offset: Int) {
        bytes[offset] = UInt8(truncatingIfNeeded: value >> 24)
        bytes[offset + 1] = UInt8(truncatingIfNeeded: value >> 16)
        bytes[offset + 2] = UInt8(truncatingIfNeeded: value >> 8)
        bytes[offset + 3] = UInt8(truncatingIfNeeded: value)
    }

    private static func read16(_ bytes: [UInt8], at offset: Int) -> UInt16 {
        UInt16(bytes[offset]) << 8 | UInt16(bytes[offset + 1])
    }

    private static func read32(_ bytes: [UInt8], at offset: Int) -> UInt32 {
        UInt32(bytes[offset]) << 24 |
            UInt32(bytes[offset + 1]) << 16 |
            UInt32(bytes[offset + 2]) << 8 |
            UInt32(bytes[offset + 3])
    }

    private static func checksum(_ bytes: [UInt8]) -> UInt16 {
        var sum: UInt32 = 0
        for offset in stride(from: 0, to: bytes.count, by: 2) {
            sum += UInt32(read16(bytes, at: offset))
            sum = (sum & 0xffff) + (sum >> 16)
        }
        return ~UInt16(truncatingIfNeeded: sum)
    }
}
