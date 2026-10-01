package app.mokuro.bunko

import java.net.Inet4Address
import java.net.NetworkInterface

object Net {
    // Cellular, VPN and virtual interfaces: other devices cannot reach the server there.
    private val skip = Regex("^(lo|rmnet|ccmni|pdp|v4-rmnet|tun|ppp|dummy|ifb|sit|ip6|clat).*")

    /**
     * Private IPv4 addresses of the interfaces other devices can reach: Wi-Fi, the
     * phone's own hotspot, Ethernet, USB/Bluetooth tethering. Wi-Fi first.
     */
    fun lanAddresses(): List<String> = try {
        NetworkInterface.getNetworkInterfaces()?.toList().orEmpty()
            .filter { it.isUp && !it.isLoopback && !skip.matches(it.name) }
            .sortedBy { if (it.name.startsWith("wlan")) 0 else 1 }
            .flatMap { nic -> nic.inetAddresses.toList().filterIsInstance<Inet4Address>() }
            .filter { !it.isLoopbackAddress && !it.isLinkLocalAddress }
            .map { it.hostAddress ?: "" }
            .filter { it.isNotEmpty() }
            .distinct()
    } catch (_: Exception) {
        emptyList()
    }

    fun url(host: String, port: Int) = "http://$host:$port/"
}
