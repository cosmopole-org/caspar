package tools

import (
	"kasper/src/abstract/adapters/docker"
	"kasper/src/abstract/adapters/elpis"
	"kasper/src/abstract/adapters/file"
	"kasper/src/abstract/adapters/firectl"
	"kasper/src/abstract/adapters/network"
	"kasper/src/abstract/adapters/security"
	"kasper/src/abstract/adapters/signaler"
	"kasper/src/abstract/adapters/storage"
	"kasper/src/abstract/adapters/wasm"
)

type ITools interface {
	Security() security.ISecurity
	Signaler() signaler.ISignaler
	Storage() storage.IStorage
	Network() network.INetwork
	File() file.IFile
	Vmm() wasm.IWasm
	Elpis() elpis.IElpis
	Docker() docker.IDocker
	Firectl() firectl.IFirectl
}
