/// The ReusePort enum is used to configure the reuse port behavior for socket binding,
/// primarily affecting the load balancing of incoming connections
/// across multiple threads or processes.
///
/// # Variants
///
/// `Disabled`: Port reuse is disabled. The socket will bind exclusively to the specified port.
///
/// `Default`: Enables port reuse using a hash-based mechanism to balance incoming connections
/// across sockets that are bound to the same port.
///
/// `CPU`: On Linux, this option attaches the socket to the CPU
/// on which the connection was handled, improving CPU locality.
/// On non-Linux platforms, this option falls back to the Default behavior,
/// where connections are balanced using a hash function.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ReusePort {
    /// Port reuse is disabled. The socket will bind exclusively to the specified port.
    Disabled,
    /// Enables port reuse using a hash-based mechanism to balance incoming connections
    /// across sockets that are bound to the same port.
    Default,
    /// `CPU`: On Linux, this option attaches the socket to the CPU
    /// on which the connection was handled, improving CPU locality.
    /// On non-Linux platforms, this option falls back to the Default behavior,
    /// where connections are balanced using a hash function.
    CPU
}

/// The BindConfig struct defines the configuration for binding sockets to addresses.
/// It allows fine-tuning of several parameters, such as enabling IPv6-only mode,
/// controlling whether the address can be reused, and configuring the port reuse mechanism.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BindConfig {
    pub backlog_size: isize,
    pub only_v6: bool,
    pub reuse_address: bool,
    pub reuse_port: ReusePort
}

impl BindConfig {
    /// Creates a new BindConfig with default values.
    pub const fn new() -> Self {
        Self {
            backlog_size: 1024,
            only_v6: false,
            reuse_address: true,
            reuse_port: ReusePort::Default,
        }
    }

    /// Sets the backlog size.
    pub fn backlog_size(mut self, backlog_size: isize) -> Self {
        self.backlog_size = backlog_size;
        self
    }

    /// Configures the socket to use only IPv6 if set to true.
    pub fn only_v6(mut self, only_v6: bool) -> Self {
        self.only_v6 = only_v6;
        self
    }

    /// Configures whether the address is reusable.
    pub fn reuse_address(mut self, reuse_address: bool) -> Self {
        self.reuse_address = reuse_address;
        self
    }

    /// Sets the [`reuse_port`](ReusePort) behavior to [`Disabled`](ReusePort::Disabled),
    /// [`Default`](ReusePort::Default), or [`CPU`](ReusePort::CPU).
    pub fn reuse_port(mut self, reuse_port: ReusePort) -> Self {
        self.reuse_port = reuse_port;
        self
    }
}

impl Default for BindConfig {
    fn default() -> Self {
        Self::new()
    }
}
