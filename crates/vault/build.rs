// sqlx::migrate!는 이미 있는 마이그레이션 파일만 추적한다. 새 파일을 더해도 다시 빌드되도록 디렉터리를 알린다
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
