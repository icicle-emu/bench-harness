# @fixme: It would nice to be actually use plain debian here
FROM ubuntu:latest

# Set basic firecracker environment
RUN apt-get update && apt-get install init udev rng-tools gdb-minimal -y \
    && apt-get autoclean && apt-get clean && apt-get -y autoremove \
    && rm -rf /var/lib/apt/lists \
    && passwd -d root
COPY fstab /etc/fstab

# Disable all services that we don't need
RUN systemctl mask \
    systemd-resolved.service \
    systemd-timesyncd.service \
    systemd-logind.service \
    modprobe@drm.service \
    NetworkManager \
    e2scrub_reap.service

# Override getty to auto-login and start our agent
COPY agent /bin/agent
COPY autologin.conf /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf

# Increase memory maps
COPY 50-limits.conf /etc/sysctl.d/50-limits.conf
