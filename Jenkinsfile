// The GIMS image: this branch's code on a GTFS data image the Nandi job made
// (nandi's Jenkinsfile, stage "GTFS Data Image"; nandi's docs/RELEASE.md). It is
// tagged <data>-<gims>, the data image's Nandi commit and this commit, so every
// (data, code) pair is its own tag - the tag a release names. GIMS is released
// in system-control-centre, not here.
//
// dataImage: the Nandi commit whose data image to build on, or `latest`. Left
// empty - as it is when a push starts the job - nothing is built.

// ------------------------------------------------------------------ where the image goes
//
// The targetEnv and targetCloud parameters choose the registries, as in the
// Nandi job:
//
//   GCP sandbox  asia-south1-docker.pkg.dev/ny-sandbox/<repo>/<repo>   gcp-sa-key
//   GCP prod     asia-south1-docker.pkg.dev/ny-prod/<repo>/<repo>      gcp-sa-key-prod
//   AWS sandbox  463356420488.dkr.ecr.ap-south-1.amazonaws.com/<repo>   (beckn-uat)
//   AWS prod     147728078333.dkr.ecr.ap-south-1.amazonaws.com/<repo>   (kurukshetra)

def addTarget(List targets, String target) {
  if (!targets.contains(target)) {
    targets.add(target)
  }
}

// Every registry this build pushes to, as "<cloud>:<env>".
def pushTargets() {
  List targets = []
  List envs = params.targetEnv == 'both' ? ['sandbox', 'prod'] : [params.targetEnv as String]
  List clouds = params.targetCloud == 'both' ? ['GCP', 'AWS'] : [params.targetCloud as String]
  for (String e : envs) {
    for (String c : clouds) {
      addTarget(targets, c + ':' + e)
    }
  }
  return targets
}

def registryFor(String target) {
  def parts = target.split(':')
  def isSandbox = parts[1] == 'sandbox'
  if (parts[0] == 'GCP') {
    return [gcp: true, host: 'asia-south1-docker.pkg.dev', project: isSandbox ? 'ny-sandbox' : 'ny-prod',
            credentialsId: isSandbox ? 'gcp-sa-key' : 'gcp-sa-key-prod']
  }
  def account = isSandbox ? env.AWS_SANDBOX_ACCOUNT : env.AWS_PROD_ACCOUNT
  return [gcp: false, host: "${account}.dkr.ecr.ap-south-1.amazonaws.com".toString()]
}

def imageRef(Map reg, String repo, String tag) {
  return reg.gcp ? "${reg.host}/${reg.project}/${repo}/${repo}:${tag}".toString() : "${reg.host}/${repo}:${tag}".toString()
}

def registryLogin(Map reg) {
  if (reg.gcp) {
    withCredentials([file(credentialsId: reg.credentialsId, variable: 'GCP_KEY_FILE')]) {
      sh 'cat $GCP_KEY_FILE | docker login -u _json_key --password-stdin https://asia-south1-docker.pkg.dev'
    }
  } else {
    sh "aws ecr get-login-password --region ap-south-1 | docker login --username AWS --password-stdin ${reg.host}"
  }
}

// Push the local image `localImage` as <repo>:<tag> to every push target. The
// local tag can vanish between two pushes (node image GC under disk pressure);
// it is then pulled back from the first registry it reached.
def pushEverywhere(String localImage, String repo, String tag) {
  Map first = null
  for (String target : pushTargets()) {
    def reg = registryFor(target)
    def ref = imageRef(reg, repo, tag)
    def src = localImage
    if (sh(script: "docker image inspect ${localImage} > /dev/null 2>&1", returnStatus: true) != 0) {
      if (first == null) {
        error "Local image ${localImage} is gone before it was pushed anywhere (${repo}:${tag})"
      }
      echo "Local image ${localImage} is gone - pulling ${first.ref} back to tag from."
      registryLogin(first.reg)
      sh "docker pull ${first.ref}"
      src = first.ref
    }
    registryLogin(reg)
    sh "docker tag ${src} ${ref}"
    sh "docker push ${ref}"
    echo "Pushed ${ref}"
    if (first == null) {
      first = [reg: reg, ref: ref]
    }
  }
}

pipeline {
  agent {
    kubernetes {
      label 'dind-agent'
    }
  }

  options {
    // one build at a time: the push relies on the local tag the build left
    disableConcurrentBuilds()
  }

  parameters {
    string(
      name: 'dataImage',
      defaultValue: '',
      description: 'The Nandi commit whose GTFS data image to build on (e.g. ae603a: gtfs-routes-service-rust:ae603a-data in ECR 147728078333, from a Nandi build with buildGimsData), or latest for the newest one there. Empty: nothing is built.'
    )
    choice(
      name: 'targetEnv',
      choices: ['sandbox', 'prod', 'both'],
      description: 'Push the image for sandbox (master), prod, or both.'
    )
    choice(
      name: 'targetCloud',
      choices: ['GCP', 'AWS', 'both'],
      description: 'Push to GCP Artifact Registry (ny-sandbox / ny-prod), AWS ECR (463356420488 sandbox / 147728078333 prod), or both.'
    )
  }

  environment {
    AWS_SANDBOX_ACCOUNT = '463356420488'
    AWS_PROD_ACCOUNT = '147728078333'
    IMAGE_NAME = 'gtfs-routes-service-rust'
  }

  stages {
    stage('Initialize') {
      steps {
        script {
          env.LAST_COMMIT_HASH = sh(script: "git rev-parse HEAD", returnStdout: true).trim().substring(0, 6)
          def data = (params.dataImage ?: '').trim()
          if (!data) {
            env.BUILD_GIMS = 'false'
            currentBuild.description = 'nothing built: no dataImage'
            echo "No dataImage given, so nothing is built. Run the job with dataImage = the Nandi commit whose data image to build on, or latest."
            return
          }
          // the data images live in the GIMS repo of ECR 147728078333
          def ecrHost = "${env.AWS_PROD_ACCOUNT}.dkr.ecr.ap-south-1.amazonaws.com"
          sh "aws ecr get-login-password --region ap-south-1 | docker login --username AWS --password-stdin ${ecrHost}"
          if (data == 'latest') {
            data = sh(
              script: """aws ecr describe-images --region ap-south-1 --registry-id ${env.AWS_PROD_ACCOUNT} --repository-name ${env.IMAGE_NAME} --filter tagStatus=TAGGED --output json | python3 -c 'import json, sys; tags = [(i["imagePushedAt"], t) for i in json.load(sys.stdin)["imageDetails"] for t in i.get("imageTags", []) if t.endswith("-data")]; print(max(tags)[1][:-len("-data")] if tags else "")'""",
              returnStdout: true
            ).trim()
            if (!data) {
              error "There is no *-data image in ECR ${env.AWS_PROD_ACCOUNT}/${env.IMAGE_NAME}: build one with the Nandi job (buildGimsData)."
            }
            echo "latest data image: ${data}"
          }
          if (!(data ==~ /[0-9a-f]{6,40}/)) {
            error "dataImage must be a Nandi commit (e.g. ae603a) or latest, not '${data}'."
          }
          env.DATA_IMAGE = "${ecrHost}/${env.IMAGE_NAME}:${data}-data"
          env.IMAGE_TAG = "${data}-${env.LAST_COMMIT_HASH}"
          env.BUILD_GIMS = 'true'
          def targets = pushTargets()
          echo "GIMS ${env.LAST_COMMIT_HASH} on ${env.DATA_IMAGE}, as ${env.IMAGE_NAME}:${env.IMAGE_TAG}, to ${targets.join(', ')}"
          currentBuild.description = "${env.IMAGE_TAG} -> ${targets.join(', ')}"
        }
      }
    }

    stage('Build') {
      when {
        expression { return env.BUILD_GIMS == 'true' }
      }
      steps {
        script {
          // reclaim disk, never an image from a build still in flight
          sh 'docker system prune -af --filter "until=24h"'
          if (sh(script: "docker pull ${env.DATA_IMAGE}", returnStatus: true) != 0) {
            error "No data image ${env.DATA_IMAGE}: build it with the Nandi job (buildGimsData) on that commit first."
          }
          sh "docker build --build-arg DATA_IMAGE=${env.DATA_IMAGE} -t ${env.IMAGE_NAME}:${env.IMAGE_TAG} ."
        }
      }
    }

    stage('Push') {
      when {
        expression { return env.BUILD_GIMS == 'true' }
      }
      steps {
        script {
          pushEverywhere("${env.IMAGE_NAME}:${env.IMAGE_TAG}", env.IMAGE_NAME, env.IMAGE_TAG)
          echo "GIMS image ${env.IMAGE_NAME}:${env.IMAGE_TAG} pushed to ${pushTargets().join(', ')}."
        }
      }
    }
  }
}
