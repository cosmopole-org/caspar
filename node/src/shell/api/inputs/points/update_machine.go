package inputs_points

import "kasper/src/shell/utils/origin"

type UpdateProgramInput struct {
	PointId     string      `json:"pointId" validate:"required"`
	MachineId   string      `json:"machineId" validate:"required"`
	ProgramMeta ProgramMeta `json:"programMeta" validate:"required"`
}

func (d UpdateProgramInput) GetData() any {
	return "dummy"
}

func (d UpdateProgramInput) GetPointId() string {
	return d.PointId
}

func (d UpdateProgramInput) Origin() string {
	return origin.FindOriginLocal(d.PointId)
}
