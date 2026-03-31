package inputs_points

import "kasper/src/shell/utils/origin"

type RemoveProgramInput struct {
	MachineId  string `json:"machineId" validate:"required"`
	PointId    string `json:"pointId" validate:"required"`
	ProgramId  string `json:"programId" validate:"required"`
	Identifier string `json:"identifier" validate:"required"`
}

func (d RemoveProgramInput) GetData() any {
	return "dummy"
}

func (d RemoveProgramInput) GetPointId() string {
	return d.PointId
}

func (d RemoveProgramInput) Origin() string {
	return origin.FindOriginLocal(d.PointId)
}
