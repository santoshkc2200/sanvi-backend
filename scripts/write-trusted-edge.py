"""Render an explicitly provisioned outer-edge network, never implicit trust."""
import ipaddress
import sys

try:
    network = ipaddress.ip_network(sys.argv[1], strict=True)
    if network.prefixlen == 0:
        raise ValueError("wildcard trust is forbidden")
except (IndexError, ValueError):
    sys.exit("SANVI_TRUSTED_EDGE_CIDR must be an explicit, non-wildcard proxy network")

print(f"set_real_ip_from {network};")
print("real_ip_header X-Forwarded-For;")
print("real_ip_recursive on;")
