package inputs_points

import "kasper/src/shell/utils/origin"

type ListPointMachinesInput struct {
	PointId string `json:"pointId" validate:"required"`
}

func (d ListPointMachinesInput) GetData() any {
	return "dummy"
}

func (d ListPointMachinesInput) GetPointId() string {
	return d.PointId
}

func (d ListPointMachinesInput) Origin() string {
	return origin.FindOriginLocal(d.PointId)
}
