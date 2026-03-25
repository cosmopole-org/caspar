package inputs_machiner

type DeployInput struct {
	MachineId    string         `json:"machineId" validate:"required"`
	EntityId     string         `json:"entityId" validate:"required"`
	EntityType   string         `json:"entityType" validate:"required"`
	Downloadable bool           `json:"downloadable"`
	Payload      string         `json:"payload" validate:"required"`
	Metadata     map[string]any `json:"metadata"`
}

func (d DeployInput) GetData() any {
	return "dummy"
}

func (d DeployInput) GetPointId() string {
	return ""
}

func (d DeployInput) Origin() string {
	return "global"
}
