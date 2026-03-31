package inputs_points

import "kasper/src/shell/utils/origin"

type UpdateProgramAccessInput struct {
	ProgramId string          `json:"programId" validate:"required"`
	PointId   string          `json:"pointId" validate:"required"`
	Access    map[string]bool `json:"access" validate:"required"`
}

func (d UpdateProgramAccessInput) GetData() any {
	return "dummy"
}

func (d UpdateProgramAccessInput) GetPointId() string {
	return d.PointId
}

func (d UpdateProgramAccessInput) Origin() string {
	return origin.FindOriginLocal(d.PointId)
}
