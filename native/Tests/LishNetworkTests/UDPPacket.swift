import Foundation

enum UDPPacket {
    static let clientPort: UInt16 = 53_000
    static let payload = Data("LISH_UDP_ECHO".utf8)

    static func datagram(destinationPort: UInt16) -> Data {
        var udp = [UInt8](repeating: 0, count: 8)
        put16(clientPort, in: &udp, at: 0)
        put16(destinationPort, in: &udp, at: 2)
        put16(UInt16(udp.count + payload.count), in: &udp, at: 4)

        var ip = [UInt8](repeating: 0, count: 20)
        ip[0] = 0x45
        put16(UInt16(ip.count + udp.count + payload.count), in: &ip, at: 2)
        put16(0x4c53, in: &ip, at: 4)
        put16(0x4000, in: &ip, at: 6)
        ip[8] = 64
        ip[9] = 17
        ip[12...15] = [10, 0, 2, 15]
        ip[16...19] = [10, 0, 2, 2]
        put16(checksum(ip), in: &ip, at: 10)

        var ethernet: [UInt8] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]
        ethernet += DHCPPacket.clientMAC
        ethernet += [0x08, 0x00]
        ethernet += ip
        ethernet += udp
        ethernet += payload
        return Data(ethernet)
    }

    static func isResponse(_ frame: Data, sourcePort: UInt16) -> Bool {
        let bytes = [UInt8](frame)
        guard bytes.count >= 14 + 20 + 8 + payload.count,
              bytes[12] == 0x08,
              bytes[13] == 0x00 else { return false }
        let ipOffset = 14
        let ipLength = Int(bytes[ipOffset] & 0x0f) * 4
        guard ipLength >= 20,
              bytes.count >= ipOffset + ipLength + 8 + payload.count,
              bytes[ipOffset + 9] == 17 else { return false }
        let udpOffset = ipOffset + ipLength
        let udpLength = Int(read16(bytes, at: udpOffset + 4))
        let payloadOffset = udpOffset + 8
        guard udpLength >= 8,
              payloadOffset + udpLength - 8 <= bytes.count else { return false }
        return read16(bytes, at: udpOffset) == sourcePort &&
            read16(bytes, at: udpOffset + 2) == clientPort &&
            Data(bytes[payloadOffset..<(payloadOffset + udpLength - 8)]) == payload
    }

    static func summary(_ frame: Data) -> String {
        let bytes = [UInt8](frame)
        guard bytes.count >= 14 else { return "ethernet length=\(bytes.count)" }
        let etherType = read16(bytes, at: 12)
        guard etherType == 0x0800, bytes.count >= 34 else {
            return String(format: "etherType=%04x length=%d", etherType, bytes.count)
        }
        let ipOffset = 14
        let ipLength = Int(bytes[ipOffset] & 0x0f) * 4
        let source = bytes[(ipOffset + 12)..<(ipOffset + 16)].map(String.init).joined(separator: ".")
        let destination = bytes[(ipOffset + 16)..<(ipOffset + 20)].map(String.init).joined(separator: ".")
        guard bytes[ipOffset + 9] == 17, bytes.count >= ipOffset + ipLength + 8 else {
            return "ip protocol=\(bytes[ipOffset + 9]) \(source)->\(destination) length=\(bytes.count)"
        }
        let udpOffset = ipOffset + ipLength
        return "udp \(source):\(read16(bytes, at: udpOffset))->\(destination):\(read16(bytes, at: udpOffset + 2)) length=\(read16(bytes, at: udpOffset + 4))/\(bytes.count)"
    }

    private static func put16(_ value: UInt16, in bytes: inout [UInt8], at offset: Int) {
        bytes[offset] = UInt8(truncatingIfNeeded: value >> 8)
        bytes[offset + 1] = UInt8(truncatingIfNeeded: value)
    }

    private static func read16(_ bytes: [UInt8], at offset: Int) -> UInt16 {
        UInt16(bytes[offset]) << 8 | UInt16(bytes[offset + 1])
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
