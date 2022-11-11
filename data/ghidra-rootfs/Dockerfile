FROM openjdk:11-jdk-slim

# Note: `afl_ghidra_bridge` and Xtensa support, appears to have issues with newer versions of Ghidra
ENV GHIDRA_URL https://github.com/NationalSecurityAgency/ghidra/releases/download/Ghidra_9.1.2_build/ghidra_9.1.2_PUBLIC_20200212.zip

RUN apt-get update && apt-get install init udev rng-tools unzip wget netcat -y \
    && wget -nv -O /tmp/ghidra.zip ${GHIDRA_URL} \
    && unzip /tmp/ghidra.zip \
    && mv ghidra_9.1.2_PUBLIC /ghidra \
    && chmod +x /ghidra/ghidraRun \
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


# Add xtensa support to ghidra
COPY xtensa /ghidra/Ghidra/Processors/xtensa
RUN /ghidra/support/sleigh -a /ghidra/Ghidra/Processors/xtensa

# Override getty to auto-login and start our agent
COPY agent /bin/agent
COPY autologin.conf /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf

# Increase memory maps
COPY 50-limits.conf /etc/sysctl.d/50-limits.conf
