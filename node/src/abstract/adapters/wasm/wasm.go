package wasm

import "kasper/src/abstract/models/worker"

type IWasm interface {
	Assign(machineId string)
	RunVm(machineId string, pointId string, data string)
	TerminateVm(machineId string)
	BuildVmImage(machineId string, imageName string, dockerfilePath string)
	ExecuteChainTrxsGroup(trxs []*worker.Trx)
	ExecuteChainEffects(effects string)
	CloseKVDB()
	WasmCallback(dataRaw string) (string, int64)
}
