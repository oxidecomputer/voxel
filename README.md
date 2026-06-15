# voxel

**VOXEL** (Virtual OXide Emulation Lab) is a testbed environment that provides a virtualized Oxide rack, realized as a set of interconnected virtual machines.

It lets operators launch a virtual rack and deploy a custom build of the Oxide control plane within it, making it a practical way to gain insight into the rack's networking and service topologies without physical hardware.

## What it's built with

1. [Falcon](https://github.com/oxidecomputer/falcon): a Rust API for creating network topologies. Falcon is the framework VOXEL is built on — it defines and launches the virtual rack (switches and gimlets) as interconnected VMs.
2. [Omicron](https://github.com/oxidecomputer/omicron): the Oxide control plane. This is what VOXEL deploys into the topology, along with Nexus, sled-agent, CockroachDB, Crucible, the Management Gateway Service (run here in simulation as `mgs-sim`), and more.
3. [SoftNPU](https://github.com/oxidecomputer/softnpu): software emulation of the switch ASIC (sidecar), standing in for the Tofino-based hardware switch so the control plane's networking can run end-to-end in a virtual environment.

## Limitations

A significant limitation of this environment is that because the compute sleds are themselves virtual machines, they cannot launch virtual machines. This is due to the
Oxide hypervisor not supporting nested virtualization. As a workaround, the environment launches probes, which take the form of a [zone](https://illumos.org/man/7/zones) that behaves similarly to a headless virtual machine instance.

## Pre-installation Steps

You'll ideally want some bare metal to deploy your a4x2 on.

### System Requirements

- **Operating System:** [Helios](https://github.com/oxidecomputer/helios)
- **Architecture:** x86_64
- **CPU:** AMD Ryzen 9 or Intel Core Ultra 9 Series
- **Memory:** 64GB RAM or more
- **Storage:** At least 500GiB of disk space on a flash storage device

### Installing Helios
Installing from a [pre-built image](https://pkg.oxide.computer/install/latest/) is straightforward, and can be done by following the instructions in the README for installing on a physical machine on the helios-engvm repository [here](https://github.com/oxidecomputer/helios-engvm#installing-on-a-physical-machine-using-the-iso).

If you will be configuring your system over a serial connection, choose the `ttya` or `ttyb` ISOs. Otherwise, if you plan to use a graphical console over HDMI or IPMI, choose `vga` ISO.

Because Helios/Illumos does not have a Fixed Release model, you can simply run `pkg update` after installing the OS in order to bring it up to date with the latest changes.

Once your system is up and running, you will want to continue following along in the `helios-engvm` README to:
- Configure networking.
- Provision account(s) - using the setup script here is optional, but worth using if you'd like for your initial user account to have the same properties and public keys as the current user on your workstation. 
- Enable services.
- Create a swap device.
- Create a dump device.
- Update system packages.

## Usage

TODO
