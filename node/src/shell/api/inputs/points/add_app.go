package inputs_points

import "kasper/src/shell/utils/origin"

type ProgramMeta struct {
	ProgramId  string          `json:"programId" validate:"required"`
	Identifier string          `json:"identifier" validate:"required"`
	Access     map[string]bool `json:"access" validate:"required"`
	Metadata   map[string]any  `json:"metadata"`
}

type AddMachineInput struct {
	MachineId    string        `json:"machineId" validate:"required"`
	PointId      string        `json:"pointId" validate:"required"`
	ProgramsMeta []ProgramMeta `json:"programsMeta" validate:"required"`
}

func (d AddMachineInput) GetData() any {
	return "dummy"
}

func (d AddMachineInput) GetPointId() string {
	return d.PointId
}

func (d AddMachineInput) Origin() string {
	return origin.FindOriginLocal(d.PointId)
}
