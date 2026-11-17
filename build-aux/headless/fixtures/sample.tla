-------------------------- MODULE sample --------------------------
(* A bounded counter shared by two processes, with a PlusCal
   algorithm, a fairness condition, and a short proof: one of each
   thing the TLA+ highlighter draws (data/language-specs/tla.lang). *)
EXTENDS Naturals, Sequences, TLC

CONSTANTS Limit, Procs
ASSUME LimitIsNat == Limit \in Nat /\ Limit > 0

(* --algorithm counter
variables count = 0, log = <<>>;

define
    InBounds == count \in 0..Limit
end define;

fair process worker \in Procs
variable seen = FALSE;
begin
  Step:
    while count < Limit do
      Incr:
        if count + 1 =< Limit then
            count := count + 1;
            log := Append(log, self);
        else
            skip;
        end if;
    end while;
  Done:
    seen := TRUE;
end process;
end algorithm; *)
\* BEGIN TRANSLATION
VARIABLES count, log, pc, seen

vars == << count, log, pc, seen >>

Init == /\ count = 0
        /\ log = << >>
        /\ seen = [self \in Procs |-> FALSE]
        /\ pc = [self \in Procs |-> "Step"]

Incr(self) == /\ pc[self] = "Incr"
              /\ IF count + 1 =< Limit
                    THEN /\ count' = count + 1
                         /\ log' = Append(log, self)
                    ELSE UNCHANGED << count, log >>
              /\ pc' = [pc EXCEPT ![self] = "Step"]
              /\ UNCHANGED seen

Next == \E self \in Procs : Incr(self)

Spec == Init /\ [][Next]_vars /\ \A self \in Procs : WF_vars(Incr(self))
\* END TRANSLATION

TypeOK == /\ count \in 0..Limit
          /\ log \in Seq(Procs)
          /\ pc \in [Procs -> {"Step", "Incr", "Done"}]

Bounded == [](count =< Limit)
Finishes == <>(\A p \in Procs : pc[p] = "Done") ~> TRUE

THEOREM Safety == Spec => []TypeOK
<1>1. Init => TypeOK
  BY DEF Init, TypeOK
<1>2. TypeOK /\ [Next]_vars => TypeOK'
  <2>1. SUFFICES ASSUME TypeOK, NEW self \in Procs, Incr(self)
                 PROVE  TypeOK'
    OBVIOUS
  <2>2. QED
    BY <2>1 DEF Incr, TypeOK
<1>3. QED
  BY <1>1, <1>2, PTL DEF Spec
=============================================================================
