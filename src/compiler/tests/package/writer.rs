use super::*;
use crate::compiler::package::PackageWriter;

fn writer_fixture() -> (TempDir, Assignment, Vec<u8>) {
    let (temp, source, assignments, checksums) = markdown_fixture();
    let package = temp.path().join("seed");
    compile(&assignments, &source, &checksums, &package).unwrap();
    let assignment = read_json(&assignments).unwrap();
    let unit = load_package(&package).unwrap().pop().unwrap();
    (temp, assignment, serde_json::to_vec(&unit).unwrap())
}

#[test]
fn writer_rejects_schema_valid_but_unbound_evidence() {
    let (temp, assignment, bytes) = writer_fixture();
    let output = temp.path().join("unbound");
    let mut writer =
        PackageWriter::new(PrivateDirectory::new(&output).unwrap(), &assignment.units).unwrap();
    let mut unit: EvidenceUnit = serde_json::from_slice(&bytes).unwrap();
    unit.sources[0].path = "unrelated.md".into();
    assert!(matches_schema(
        "evidence-unit.schema.json",
        &serde_json::to_value(&unit).unwrap()
    ));
    let error = writer.write(0, unit).unwrap_err().to_string();
    assert!(
        error.contains("source receipts do not match the locator"),
        "{error}"
    );
    drop(writer);
    assert!(!output.exists());
    assert!(!temp.path().join("unbound.staging").exists());
}

#[test]
fn writer_owns_assignment_membership_and_completion_order() {
    let (temp, mut assignment, bytes) = writer_fixture();
    let mut second: AssignedUnit =
        serde_json::from_value(serde_json::to_value(&assignment.units[0]).unwrap()).unwrap();
    second.unit_id = "markdown:second".into();
    assignment.units.push(second);
    let evidence = |id: &str| {
        let mut unit: EvidenceUnit = serde_json::from_slice(&bytes).unwrap();
        unit.unit_id = id.into();
        unit
    };
    let first_id = &assignment.units[0].unit_id;
    let output = temp.path().join("ordered");
    let mut writer =
        PackageWriter::new(PrivateDirectory::new(&output).unwrap(), &assignment.units).unwrap();
    assert!(writer.write(1, evidence(first_id)).is_err());
    writer.write(1, evidence("markdown:second")).unwrap();
    assert!(writer.write(1, evidence("markdown:second")).is_err());
    writer.write(0, evidence(first_id)).unwrap();
    writer.finish().unwrap();
    let units = load_package(&output).unwrap();
    assert_eq!(units[0].unit_id, *first_id);
    assert_eq!(units[1].unit_id, "markdown:second");

    let output = temp.path().join("incomplete");
    let mut writer =
        PackageWriter::new(PrivateDirectory::new(&output).unwrap(), &assignment.units).unwrap();
    writer.write(0, evidence(first_id)).unwrap();
    assert!(writer.finish().is_err());
    assert!(!output.exists());
    assert!(!temp.path().join("incomplete.staging").exists());

    assignment.units[1].unit_id = assignment.units[0].unit_id.clone();
    let output = temp.path().join("duplicate");
    assert!(
        PackageWriter::new(PrivateDirectory::new(&output).unwrap(), &assignment.units).is_err()
    );
    assert!(!output.exists());
    assert!(!temp.path().join("duplicate.staging").exists());
}
