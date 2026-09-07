use crate::drivers::vmm::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VmPacketKind {
    RunVm,
    TerminateVm,
    DeleteVm,
    ExecVm,
    CopyToVm,
    BuildVmImage,
    HostCall,
    VerifyProgramExecution,
    ForwardHttp,
    ApiResponse,
    BridgeApi,
    Unknown,
}

impl VmPacketKind {
    pub(crate) fn from_packet(packet: &JsonValue) -> Self {
        match packet["type"].as_str().unwrap_or("") {
            "runVm" => Self::RunVm,
            "terminateVm" => Self::TerminateVm,
            "deleteVm" | "destroyVm" => Self::DeleteVm,
            "execVm" | "execDocker" => Self::ExecVm,
            "copyToVm" | "copyToDocker" => Self::CopyToVm,
            "buildVmImage" | "buildDockerImage" => Self::BuildVmImage,
            "hostCall" => Self::HostCall,
            "verifyProgramExecution" | "elpifyProof" => Self::VerifyProgramExecution,
            "forwardHttp" => Self::ForwardHttp,
            "apiResponse" => Self::ApiResponse,
            "bridgeApi" => Self::BridgeApi,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VmPacketContext {
    pub(crate) packet_type: VmPacketKind,
    pub(crate) runtime: String,
    pub(crate) machine_id: String,
    pub(crate) vm_id: String,
}

impl VmPacketContext {
    pub(crate) fn from_packet(packet: &JsonValue) -> Self {
        VmPacketContext {
            packet_type: VmPacketKind::from_packet(packet),
            runtime: packet["runtime"].as_str().unwrap_or("").to_lowercase(),
            machine_id: packet["machineId"].as_str().unwrap_or("").to_string(),
            vm_id: packet["vmId"].as_str().unwrap_or("main").to_string(),
        }
    }
}
