#!/bin/bash
# ref docker-x11base//rootfs/buildx.sh
source /etc/profile
export |grep DOCKER_REG |grep -Ev "PASS|PW"
repo=registry.cn-shenzhen.aliyuncs.com
echo "${DOCKER_REGISTRY_PW_infrastSubUser2}" |docker login --username=${DOCKER_REGISTRY_USER_infrastSubUser2} --password-stdin $repo
repoHub=docker.io
echo "${DOCKER_REGISTRY_PW_dockerhub}" |docker login --username=${DOCKER_REGISTRY_USER_dockerhub} --password-stdin $repoHub


function doBuildx(){
    local tag=$1
    local dockerfile=$2

    repo=registry-1.docker.io
    # repo=registry.cn-shenzhen.aliyuncs.com
    test ! -z "$REPO" && repo=$REPO #@gitac
    img="fxa-pushbox:$tag"
    # cache
    ali="registry.cn-shenzhen.aliyuncs.com"
    cimg="$img-cache"
    
    # plat="--platform linux/amd64,linux/arm64,linux/arm" #,linux/arm
    plat="--platform linux/amd64" #

    compile="alpine-compile"; builddate=$(date +%Y-%m-%d_%H:%M:%S)
    # test "$plat" != "--platform linux/amd64,linux/arm64,linux/arm" && compile="${compile}-dbg"
    # --build-arg REPO=$repo/ #temp notes, just use dockerHub's
    args="""
    --provenance=false 
    --build-arg REPO=$repo/
    --build-arg COMPILE_IMG=$compile
    --build-arg NOCACHE=$builddate
    --build-arg BUILDDATE=$builddate
    """
    
    # ref :3000/sam/quickstart-dockerfile >> mode=max
    #  oci-mediatypes=true,image-manifest=true,:ali-403-forbidden
    ali2=$REPO_TEN_HK
    cache="--cache-from type=registry,ref=$ali2/$ns/$cimg"
    cache="$cache --cache-to type=registry,ref=$ali2/$ns/$cimg,mode=max"
    
    output="--output type=image,name=$repo/$ns/$img,push=true,oci-mediatypes=true,annotation.author=sam"
    docker buildx build $cache $plat $args $output -f $dockerfile . 
}

ns=infrastlabs
case "$1" in
pushbox)
    doBuildx latest Dockerfile
    ;;
*)
    echo "emp params"
    ;;          
esac