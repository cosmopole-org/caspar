package inputs_points

import "kasper/src/shell/utils/origin"

type AddProgramInput struct {
	MachineId   string      `json:"machineId" validate:"required"`
	PointId     string      `json:"pointId" validate:"required"`
	ProgramMeta ProgramMeta `json:"programMeta" validate:"required"`
}

func (d AddProgramInput) GetData() any {
	return "dummy"
}

func (d AddProgramInput) GetPointId() string {
	return d.PointId
}

func (d AddProgramInput) Origin() string {
	return origin.FindOriginLocal(d.PointId)
}
