# Multirack DDM quick test

Branch: skaram/multirack-interconnect
Config: racks = 2 (BGP or static; transit_bfd optional)
Nodes: g0 = rack1 switch0, g3 = rack2 switch0
Prefixes: rack1 /56 = fd00:17:1:d00::/56, rack2 /56 = fd00:17:1:e00::/56
Proves: RFD 583 section 6.1 (transit-transit session + /56 propagation)

## 1. Launch

```
voxel launch
```

Rack1 reaches "rack initialized". Rack2 stops before RSS.
Front interconnect ports (qsfp2) come up link-local on both switches.

## 2. Start a throwaway transit ddmd on the front port (both switches)

mg-ddm runs DDM on rear ports only, so the front port needs its own instance.

```
voxel tp exec -c "/opt/oxide/mg-ddm/bin/ddmd --kind transit --admin-port 8001 --exchange-port 56798 -a tfportqsfp2_0/ll &" g0
voxel tp exec -c "/opt/oxide/mg-ddm/bin/ddmd --kind transit --admin-port 8001 --exchange-port 56798 -a tfportqsfp2_0/ll &" g3
```

## 3. Rear-port peers (production mg-ddm, port 8000)

```
voxel tp exec -c "/opt/oxide/mg-ddm/bin/ddmadm get-peers" g0
```

```
Interface  Host  Address                 Kind    Status    Duration
10         g2    fe80::aa40:25ff:fe00:5  Server  Exchange  9h 12m 41s
8          g0    fe80::aa40:25ff:fe00:1  Server  Exchange  9h 12m 41s
9          g1    fe80::aa40:25ff:fe00:3  Server  Exchange  9h 12m 41s
```

## 4. Front-port transit peers (throwaway ddmd, port 8001)

```
voxel tp exec -c "/opt/oxide/mg-ddm/bin/ddmadm -p 8001 get-peers" g0
```

```
Interface  Host        Address                    Kind     Status    Duration
6          oxz_switch  fe80::aa40:25ff:fe65:87f3  Transit  Exchange  6m 21s 7ms
```

```
voxel tp exec -c "/opt/oxide/mg-ddm/bin/ddmadm -p 8001 get-peers" g3
```

```
Interface  Host        Address                    Kind     Status    Duration
8          oxz_switch  fe80::aa40:25ff:fe16:351a  Transit  Exchange  17m 32s 480ms
```

## 5. Advertise rack2 prefixes from rack2

```
voxel tp exec -c "/opt/oxide/mg-ddm/bin/ddmadm -p 8001 advertise-prefixes fd00:17:1:e00::/56 fd00:dead:beef::/64" g3
```

## 6. Confirm originated on rack2

```
voxel tp exec -c "/opt/oxide/mg-ddm/bin/ddmadm -p 8001 get-originated" g3
```

```
Prefix
fd00:dead:beef::/64
fd00:17:1:e00::/56
```

## 7. Confirm learned on rack1

```
voxel tp exec -c "/opt/oxide/mg-ddm/bin/ddmadm -p 8001 get-prefixes" g0
```

```
Destination          Next Hop                   Path
fd00:17:1:e00::/56   fe80::aa40:25ff:fe65:87f3  oxz_switch
fd00:dead:beef::/64  fe80::aa40:25ff:fe65:87f3  oxz_switch
```

## Notes

- Control plane only: session + prefix propagation. No data-plane forwarding.
- Data-plane forwarding: run ddmd with --dendrite plus a live originator behind rack2 (blocked while rack2 is pre-RSS).
- --exchange-port must differ from production ddmd (56797). Do not pass --with-stats (its oximeter port also defaults to 8001).
- RFD 573: unify intra/inter-rack on one ddmd (future work).
