package inputs_points

import (
	"testing"

	"kasper/src/abstract/models/input"
)

func TestPointsInputsImplementInterface(t *testing.T) {
	var _ input.IInput = UpdateMemberAccessInput{}
	var _ input.IInput = DeleteInput{}
	var _ input.IInput = HistoryInput{}
	var _ input.IInput = RemoveMemberInput{}
	var _ input.IInput = AddMachineInput{}
	var _ input.IInput = UpdateMemberInput{}
	var _ input.IInput = ListInput{}
	var _ input.IInput = JoinInput{}
	var _ input.IInput = MetaInput{}
	var _ input.IInput = RemoveMachineInput{}
	var _ input.IInput = SignalInput{}
	var _ input.IInput = GetDefaultAccessInput{}
	var _ input.IInput = AddProgramInput{}
	var _ input.IInput = ListPointMachinesInput{}
	var _ input.IInput = GlobalHistoryInput{}
	var _ input.IInput = AddMemberInput{}
	var _ input.IInput = GetInput{}
	var _ input.IInput = CreateInput{}
	var _ input.IInput = ReadInput{}
	var _ input.IInput = UpdateProgramInput{}
	var _ input.IInput = UpdateProgramAccessInput{}
	var _ input.IInput = RemoveProgramInput{}
	var _ input.IInput = ReadMemberInput{}
	var _ input.IInput = UpdateInput{}
}

func TestPointsOriginDerivation(t *testing.T) {
	point := "point@origin-x"
	localOriginPointInputs := []input.IInput{
		UpdateMemberAccessInput{PointId: point}, DeleteInput{PointId: point}, RemoveMemberInput{PointId: point},
		AddMachineInput{PointId: point}, UpdateMemberInput{PointId: point}, JoinInput{PointId: point},
		RemoveMachineInput{PointId: point}, AddProgramInput{PointId: point}, ListPointMachinesInput{PointId: point},
		AddMemberInput{PointId: point}, GetInput{PointId: point}, UpdateProgramInput{PointId: point},
		UpdateProgramAccessInput{PointId: point}, RemoveProgramInput{PointId: point}, ReadMemberInput{PointId: point},
		UpdateInput{PointId: point},
	}
	for i, in := range localOriginPointInputs {
		if in.GetPointId() != point {
			t.Fatalf("point-based case %d got point=%q", i, in.GetPointId())
		}
		if in.Origin() != "origin-x" {
			t.Fatalf("point-based case %d got origin=%q", i, in.Origin())
		}
	}

	if m := (MetaInput{PointId: "point@global"}).Origin(); m != "" {
		t.Fatalf("meta global should map to empty, got %q", m)
	}
	if got := (CreateInput{Orig: "global"}).Origin(); got != "" {
		t.Fatalf("create origin global should map to empty, got %q", got)
	}
	if got := (CreateInput{Orig: "origin-y"}).Origin(); got != "origin-y" {
		t.Fatalf("create origin mismatch got %q", got)
	}
	if r := (ReadInput{Orig: "origin-z"}); r.GetPointId() != "" || r.Origin() != "origin-z" {
		t.Fatalf("read input mismatch point=%q origin=%q", r.GetPointId(), r.Origin())
	}
	if s := (SignalInput{PointId: point}); s.Origin() != "" {
		t.Fatalf("signal origin should be empty, got %q", s.Origin())
	}
	if h := (HistoryInput{PointId: point}); h.Origin() != "" {
		t.Fatalf("history origin should be empty, got %q", h.Origin())
	}
	if g := (GlobalHistoryInput{}).Origin(); g != "" {
		t.Fatalf("global history origin mismatch: %q", g)
	}
	if l := (ListInput{}).Origin(); l != "" {
		t.Fatalf("list origin mismatch: %q", l)
	}
	if d := (GetDefaultAccessInput{}).Origin(); d != "" {
		t.Fatalf("default access origin mismatch: %q", d)
	}
}
